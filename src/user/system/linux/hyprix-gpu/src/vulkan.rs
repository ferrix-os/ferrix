//! The [`Backend`] that records Vulkan command buffers.
//!
//! The GPU painter's quads, shaders and constants (`compositor_render::gpu`)
//! drawn on the device tole picks (`tole::gpu::Core`): the same GLSL the
//! virgl backend runs, compiled to SPIR-V (`tools/regenerate.sh`), its
//! `u[]` and `to_clip` pushed as constants.
//!
//! # Layouts and barriers
//!
//! Every image lives in `GENERAL`: drawn into, sampled, copied to and from,
//! and read by the card, so no image is ever in the wrong layout for what
//! comes next. Order is kept by one full memory barrier wherever the
//! command buffer changes what it does -- before each rendering begins
//! (a new target), before each copy, and before the frame's end -- which
//! covers every write a later command reads. The painter never samples the
//! image it draws into, so nothing inside one rendering needs a barrier.
//!
//! # Frames
//!
//! [`Backend::flush`] submits and waits: one frame in flight, which is what
//! the protocol's version 1 asks for (`Done` once the GPU has finished).
//! Vertices and uploads are written into host buffers that are begun again
//! after each wait.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

use ash::vk;
use compositor_render::gpu::{Backend, Blend, Filter, Layout, Program, Region, Use};
use tole::gpu::Core;

/// What every image holds: four bytes a pixel, blue first.
pub const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// `DRM_FORMAT_MOD_LINEAR`.
pub const LINEAR: u64 = 0;

/// `DRM_FORMAT_MOD_INVALID`, which an output says when it is no dmabuf but
/// plain shared memory (a memfd): what a device without dmabuf export --
/// lavapipe on Ferrix -- draws the frame into, and the card is handed
/// as a copy, as a `wl_shm` buffer is.
pub const MEMFD: u64 = compositor_gpu_protocol::MODIFIER_MEMFD;

/// Bytes of the push constants: `to_clip`, then `u[4]`.
const CONSTANT_BYTES: u32 = 80;

/// How long a submission is waited for before the device is called lost.
const WAIT_NS: u64 = 5_000_000_000;

/// The SPIR-V of the one vertex shader.
const VERT: &[u8] = include_bytes!("spirv/quad.vert.spv");

/// The SPIR-V of a fragment shader.
const fn fragment(program: Program) -> &'static [u8] {
    match program {
        Program::Solid => include_bytes!("spirv/solid.frag.spv"),
        Program::Surface => include_bytes!("spirv/surface.frag.spv"),
        Program::Gradient => include_bytes!("spirv/gradient.frag.spv"),
        Program::Shadow => include_bytes!("spirv/shadow.frag.spv"),
        Program::BlurPrepare => include_bytes!("spirv/blur_prepare.frag.spv"),
        Program::BlurDown => include_bytes!("spirv/blur_down.frag.spv"),
        Program::BlurUp => include_bytes!("spirv/blur_up.frag.spv"),
        Program::BlurFinish => include_bytes!("spirv/blur_finish.frag.spv"),
    }
}

/// `what` failed with `result`, as an error.
fn said(what: &str, result: vk::Result) -> io::Error {
    io::Error::other(format!("{what}: {result:?}"))
}

/// SPIR-V bytes as words.
fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .filter_map(|w| <[u8; 4]>::try_from(w).ok())
        .map(u32::from_le_bytes)
        .collect()
}

/// The whole of a colour image with one level and one layer.
const fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// One colour layer, for copies.
const fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// An image and what reads it.
#[derive(Debug)]
struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    /// Read as it is, and with alpha as one.
    views: [vk::ImageView; 2],
    /// The sets sampling it, made when first used: `[opaque][filter]`.
    sets: [[vk::DescriptorSet; 2]; 2],
    width: u32,
    height: u32,
}

/// An output in a memfd: the device's buffer over this process's mapping
/// of it.
#[derive(Debug)]
struct HostOutput {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    len: usize,
    /// Bytes from one row to the next.
    stride: u32,
    width: u32,
    height: u32,
}

/// A host-visible buffer, mapped.
#[derive(Debug)]
struct HostBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    size: u64,
}

/// A dmabuf the device exported: what the card is handed.
#[derive(Debug)]
pub struct Exported {
    /// The image's number.
    pub id: u32,
    /// The descriptor.
    pub fd: OwnedFd,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Where the first row is.
    pub offset: u32,
    /// The layout's modifier.
    pub modifier: u64,
    /// The property flags of the memory type it was made in.
    pub memory: vk::MemoryPropertyFlags,
}

/// The device, the pipelines, the images, and the command buffer being
/// recorded.
pub struct Vulkan {
    images: HashMap<u32, Image>,
    next_id: u32,
    /// Images let go of while a recording may still use them.
    doomed: Vec<Image>,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    descriptors: vk::DescriptorPool,
    samplers: [vk::Sampler; 2],
    vertex_module: vk::ShaderModule,
    fragment_modules: HashMap<Program, vk::ShaderModule>,
    pipelines: HashMap<(Program, Blend), vk::Pipeline>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Whether `cmd` is recording.
    recording: bool,
    /// The image drawn into, and whether a rendering into it is open.
    target: Option<u32>,
    rendering: bool,
    /// What the next draw is drawn with.
    program: Option<(Program, Blend)>,
    constants: [f32; 16],
    sampled: Option<(u32, bool, Filter)>,
    vertices: Option<HostBuffer>,
    vertex_at: u64,
    staging: Option<HostBuffer>,
    staging_at: u64,
    readback: Option<HostBuffer>,
    image_drm: Option<ash::ext::image_drm_format_modifier::Device>,
    /// The device's memory types.
    memory: vk::PhysicalDeviceMemoryProperties,
    /// Outputs in host-visible memory, mapped: where they start, how long.
    mapped: HashMap<u32, (*mut u8, usize)>,
    /// Outputs in memfds, by number.
    host_outputs: HashMap<u32, HostOutput>,
    failed: Option<io::Error>,
    /// Whether the device was lost: nothing more is drawn.
    lost: bool,
    /// Dropped last: it destroys the device.
    core: Core,
}

impl std::fmt::Debug for Vulkan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vulkan")
            .field("device", &self.core.name())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

impl Vulkan {
    /// The backend on `core`'s device.
    ///
    /// # Errors
    ///
    /// The device's, for the layouts, samplers, shaders and command buffer.
    pub fn new(core: Core) -> io::Result<Self> {
        // SAFETY: the physical device is the instance's.
        let memory = unsafe {
            core.instance()
                .get_physical_device_memory_properties(core.physical())
        };
        let image_drm = core.dmabuf().is_some().then(|| {
            ash::ext::image_drm_format_modifier::Device::new(core.instance(), core.device())
        });
        let mut vulkan = Self {
            images: HashMap::new(),
            next_id: 1,
            doomed: Vec::new(),
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            descriptors: vk::DescriptorPool::null(),
            samplers: [vk::Sampler::null(); 2],
            vertex_module: vk::ShaderModule::null(),
            fragment_modules: HashMap::new(),
            pipelines: HashMap::new(),
            pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            recording: false,
            target: None,
            rendering: false,
            program: None,
            constants: [0.0; 16],
            sampled: None,
            vertices: None,
            vertex_at: 0,
            staging: None,
            staging_at: 0,
            readback: None,
            image_drm,
            memory,
            mapped: HashMap::new(),
            host_outputs: HashMap::new(),
            failed: None,
            lost: false,
            core,
        };
        vulkan.make_parts()?;
        Ok(vulkan)
    }

    /// The device tole picked.
    #[must_use]
    pub const fn core(&self) -> &Core {
        &self.core
    }

    /// Whether the device was lost.
    #[must_use]
    pub const fn is_lost(&self) -> bool {
        self.lost
    }

    /// The device.
    fn device(&self) -> &ash::Device {
        self.core.device()
    }

    /// The layouts, samplers, shader modules and the command buffer.
    fn make_parts(&mut self) -> io::Result<()> {
        let device = self.core.device().clone();
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: one sampled-image binding.
        self.set_layout = unsafe { device.create_descriptor_set_layout(&info, None) }
            .map_err(|e| said("vkCreateDescriptorSetLayout", e))?;
        let layouts = [self.set_layout];
        let push = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: CONSTANT_BYTES,
        }];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        // SAFETY: the set layout was just made.
        self.pipeline_layout = unsafe { device.create_pipeline_layout(&info, None) }
            .map_err(|e| said("vkCreatePipelineLayout", e))?;
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 8192,
        }];
        let info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(8192)
            .pool_sizes(&sizes);
        // SAFETY: a pool of sampled-image sets.
        self.descriptors = unsafe { device.create_descriptor_pool(&info, None) }
            .map_err(|e| said("vkCreateDescriptorPool", e))?;
        for (at, filter) in [vk::Filter::NEAREST, vk::Filter::LINEAR]
            .into_iter()
            .enumerate()
        {
            let info = vk::SamplerCreateInfo::default()
                .mag_filter(filter)
                .min_filter(filter)
                .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .max_lod(0.0);
            // SAFETY: a clamped sampler, no anisotropy.
            let sampler = unsafe { device.create_sampler(&info, None) }
                .map_err(|e| said("vkCreateSampler", e))?;
            if let Some(slot) = self.samplers.get_mut(at) {
                *slot = sampler;
            }
        }
        self.vertex_module = module(&device, VERT)?;
        for program in Program::ALL {
            let made = module(&device, fragment(program))?;
            let _ = self.fragment_modules.insert(program, made);
        }
        let info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.core.family())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the family is the device's.
        self.pool = unsafe { device.create_command_pool(&info, None) }
            .map_err(|e| said("vkCreateCommandPool", e))?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the pool was just made.
        self.cmd = unsafe { device.allocate_command_buffers(&alloc) }
            .map_err(|e| said("vkAllocateCommandBuffers", e))?
            .first()
            .copied()
            .unwrap_or_default();
        // SAFETY: a plain fence.
        self.fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| said("vkCreateFence", e))?;
        Ok(())
    }

    /// Remember the first failure.
    fn fail(&mut self, error: io::Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    /// The pipeline drawing `program` mixed as `blend`, made the first time.
    fn pipeline(&mut self, program: Program, blend: Blend) -> io::Result<vk::Pipeline> {
        if let Some(pipeline) = self.pipelines.get(&(program, blend)) {
            return Ok(*pipeline);
        }
        let frag = self
            .fragment_modules
            .get(&program)
            .copied()
            .ok_or_else(|| io::Error::other("no shader"))?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let bindings = [vk::VertexInputBindingDescription {
            binding: 0,
            stride: 16,
            input_rate: vk::VertexInputRate::VERTEX,
        }];
        let attributes = [
            vk::VertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 0,
            },
            vk::VertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 8,
            },
        ];
        let vertex = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let attachment = match blend {
            Blend::Over => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Replace => vk::PipelineColorBlendAttachmentState::default().blend_enable(false),
        }
        .color_write_mask(vk::ColorComponentFlags::RGBA);
        let attachments = [attachment];
        let blending = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let formats = [FORMAT];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blending)
            .dynamic_state(&dynamic)
            .layout(self.pipeline_layout)
            .push_next(&mut rendering)];
        // SAFETY: every state the pipeline needs is given, its format is
        // what it draws into, and the modules live on.
        let made = unsafe {
            self.device()
                .create_graphics_pipelines(vk::PipelineCache::null(), &info, None)
        }
        .map_err(|(_, e)| said("vkCreateGraphicsPipelines", e))?;
        let pipeline = made.first().copied().unwrap_or_default();
        let _ = self.pipelines.insert((program, blend), pipeline);
        Ok(pipeline)
    }

    /// Start recording, if not recording yet.
    fn record(&mut self) -> io::Result<()> {
        if self.recording {
            return Ok(());
        }
        // SAFETY: the buffer is not pending: every submission is waited for.
        unsafe {
            self.device()
                .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
        }
        .map_err(|e| said("vkResetCommandBuffer", e))?;
        let info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer was just reset.
        unsafe { self.device().begin_command_buffer(self.cmd, &info) }
            .map_err(|e| said("vkBeginCommandBuffer", e))?;
        self.recording = true;
        Ok(())
    }

    /// End the rendering, if one is open.
    fn close(&mut self) {
        if self.rendering {
            // SAFETY: a rendering is open on the recording buffer.
            unsafe { self.device().cmd_end_rendering(self.cmd) };
            self.rendering = false;
        }
    }

    /// Every write before it visible to every command after it.
    fn barrier(&self) {
        let all = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)];
        let info = vk::DependencyInfo::default().memory_barriers(&all);
        // SAFETY: the buffer is recording, outside a rendering.
        unsafe { self.device().cmd_pipeline_barrier2(self.cmd, &info) };
    }

    /// Ready to copy: recording, no rendering open, earlier writes done.
    fn ready_to_copy(&mut self) -> io::Result<()> {
        self.record()?;
        self.close();
        self.barrier();
        Ok(())
    }

    /// `image` from `UNDEFINED` to `GENERAL`, on the recording buffer.
    fn make_general(&mut self, image: vk::Image) -> io::Result<()> {
        self.ready_to_copy()?;
        let barriers = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range())];
        let info = vk::DependencyInfo::default().image_memory_barriers(&barriers);
        // SAFETY: recording, outside a rendering; the image is this
        // device's and new.
        unsafe { self.device().cmd_pipeline_barrier2(self.cmd, &info) };
        Ok(())
    }

    /// A host-visible buffer of at least `size` bytes.
    fn host_buffer(&self, size: u64, usage: vk::BufferUsageFlags) -> io::Result<HostBuffer> {
        let size = size.max(4096);
        let info = vk::BufferCreateInfo::default().size(size).usage(usage);
        // SAFETY: a plain exclusive buffer.
        let buffer = unsafe { self.device().create_buffer(&info, None) }
            .map_err(|e| said("vkCreateBuffer", e))?;
        // SAFETY: the buffer is this device's.
        let needs = unsafe { self.device().get_buffer_memory_requirements(buffer) };
        let visible =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let kind = self
            .core
            .memory_type(
                needs.memory_type_bits,
                visible | vk::MemoryPropertyFlags::HOST_CACHED,
            )
            .or_else(|| self.core.memory_type(needs.memory_type_bits, visible));
        let Some(kind) = kind else {
            // SAFETY: unused.
            unsafe { self.device().destroy_buffer(buffer, None) };
            return Err(io::Error::other("no host-visible memory"));
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(needs.size)
            .memory_type_index(kind);
        // SAFETY: the buffer's own requirements.
        let memory = match unsafe { self.device().allocate_memory(&alloc, None) } {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_buffer(buffer, None) };
                return Err(said("vkAllocateMemory", e));
            }
        };
        let fail = |e: vk::Result, what: &str| {
            // SAFETY: unused.
            unsafe { self.device().destroy_buffer(buffer, None) };
            // SAFETY: the buffer bound to it is gone.
            unsafe { self.device().free_memory(memory, None) };
            said(what, e)
        };
        // SAFETY: bound once at offset 0.
        if let Err(e) = unsafe { self.device().bind_buffer_memory(buffer, memory, 0) } {
            return Err(fail(e, "vkBindBufferMemory"));
        }
        // SAFETY: host-visible memory, mapped whole once.
        let ptr = match unsafe {
            self.device()
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
        } {
            Ok(ptr) => ptr.cast::<u8>(),
            Err(e) => return Err(fail(e, "vkMapMemory")),
        };
        Ok(HostBuffer {
            buffer,
            memory,
            ptr,
            size,
        })
    }

    /// Let a host buffer go. The device must be done with it.
    fn destroy_buffer(&self, buffer: HostBuffer) {
        // SAFETY: nothing pending uses it.
        unsafe { self.device().destroy_buffer(buffer.buffer, None) };
        // SAFETY: the buffer bound to it is gone; freeing unmaps.
        unsafe { self.device().free_memory(buffer.memory, None) };
    }

    /// Room for `len` more bytes in the buffer in `which`, written from
    /// `at`; a full one is grown only when nothing recorded uses it.
    fn room(&mut self, which: Which, len: u64) -> io::Result<()> {
        let (slot, at) = match which {
            Which::Vertices => (&self.vertices, self.vertex_at),
            Which::Staging => (&self.staging, self.staging_at),
        };
        let needed = at.saturating_add(len);
        if slot.as_ref().is_some_and(|b| b.size >= needed) {
            return Ok(());
        }
        // A buffer the recording reads cannot be replaced under it: run
        // what is recorded first, which empties it.
        if at > 0 {
            self.submit_and_wait()?;
        }
        let usage = match which {
            Which::Vertices => vk::BufferUsageFlags::VERTEX_BUFFER,
            Which::Staging => vk::BufferUsageFlags::TRANSFER_SRC,
        };
        let size = len.saturating_add(len / 2).next_power_of_two().max(1 << 20);
        let made = self.host_buffer(size, usage)?;
        let old = match which {
            Which::Vertices => self.vertices.replace(made),
            Which::Staging => self.staging.replace(made),
        };
        if let Some(old) = old {
            self.destroy_buffer(old);
        }
        Ok(())
    }

    /// End, submit and wait for the recording, and begin the buffers again.
    fn submit_and_wait(&mut self) -> io::Result<()> {
        if !self.recording {
            return Ok(());
        }
        self.close();
        self.barrier();
        self.recording = false;
        self.target_dirty();
        // SAFETY: the buffer is recording.
        unsafe { self.device().end_command_buffer(self.cmd) }
            .map_err(|e| said("vkEndCommandBuffer", e))?;
        let cmds = [self.cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        // SAFETY: the fence is unsignalled; the buffer is complete.
        unsafe {
            self.device()
                .queue_submit(self.core.queue(), &submit, self.fence)
        }
        .map_err(|e| self.lost_if(e, "vkQueueSubmit"))?;
        // SAFETY: the fence was submitted just now.
        let waited = unsafe { self.device().wait_for_fences(&[self.fence], true, WAIT_NS) };
        if let Err(e) = waited {
            // A wait that ran out is a device that is not coming back.
            self.lost = true;
            return Err(said("vkWaitForFences", e));
        }
        // SAFETY: signalled, and no submission names it.
        unsafe { self.device().reset_fences(&[self.fence]) }
            .map_err(|e| said("vkResetFences", e))?;
        self.vertex_at = 0;
        self.staging_at = 0;
        for image in std::mem::take(&mut self.doomed) {
            self.destroy_image(image);
        }
        Ok(())
    }

    /// `e` from `what`, remembering a lost device.
    fn lost_if(&mut self, e: vk::Result, what: &str) -> io::Error {
        if e == vk::Result::ERROR_DEVICE_LOST {
            self.lost = true;
        }
        said(what, e)
    }

    /// Forget what is bound: a new recording binds again.
    fn target_dirty(&mut self) {
        self.rendering = false;
    }

    /// Let an image go. The device must be done with it.
    fn destroy_image(&self, image: Image) {
        for set in image.sets.iter().flatten() {
            if *set != vk::DescriptorSet::null() {
                // SAFETY: the set came from this pool, which frees sets,
                // and no pending command uses it.
                let _ = unsafe {
                    self.device()
                        .free_descriptor_sets(self.descriptors, &[*set])
                };
            }
        }
        for view in image.views {
            if view != vk::ImageView::null() {
                // SAFETY: nothing pending uses it.
                unsafe { self.device().destroy_image_view(view, None) };
            }
        }
        // SAFETY: as above.
        unsafe { self.device().destroy_image(image.image, None) };
        // SAFETY: the image bound to it is gone.
        unsafe { self.device().free_memory(image.memory, None) };
    }

    /// The two views of `image`.
    fn views(&self, image: vk::Image) -> io::Result<[vk::ImageView; 2]> {
        let mut views = [vk::ImageView::null(); 2];
        for (at, opaque) in [false, true].into_iter().enumerate() {
            let alpha = if opaque {
                vk::ComponentSwizzle::ONE
            } else {
                vk::ComponentSwizzle::IDENTITY
            };
            let info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(FORMAT)
                .components(vk::ComponentMapping {
                    r: vk::ComponentSwizzle::IDENTITY,
                    g: vk::ComponentSwizzle::IDENTITY,
                    b: vk::ComponentSwizzle::IDENTITY,
                    a: alpha,
                })
                .subresource_range(color_range());
            // SAFETY: a 2D colour image of FORMAT on this device.
            match unsafe { self.device().create_image_view(&info, None) } {
                Ok(view) => {
                    if let Some(slot) = views.get_mut(at) {
                        *slot = view;
                    }
                }
                Err(e) => {
                    for view in views.into_iter().filter(|v| *v != vk::ImageView::null()) {
                        // SAFETY: unused.
                        unsafe { self.device().destroy_image_view(view, None) };
                    }
                    return Err(said("vkCreateImageView", e));
                }
            }
        }
        Ok(views)
    }

    /// Keep `image`, bound to `memory`, under a new number, in `GENERAL`.
    fn keep(
        &mut self,
        image: vk::Image,
        memory: vk::DeviceMemory,
        (width, height): (u32, u32),
    ) -> io::Result<u32> {
        let views = match self.views(image) {
            Ok(views) => views,
            Err(error) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_image(image, None) };
                // SAFETY: the image is gone.
                unsafe { self.device().free_memory(memory, None) };
                return Err(error);
            }
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let _ = self.images.insert(
            id,
            Image {
                image,
                memory,
                views,
                sets: [[vk::DescriptorSet::null(); 2]; 2],
                width,
                height,
            },
        );
        self.make_general(image)?;
        Ok(id)
    }

    /// Bind `image` to new memory of the device's own, dedicated, made
    /// for export when `export`.
    fn bind_new(
        &self,
        image: vk::Image,
        export: bool,
        wanted: vk::MemoryPropertyFlags,
    ) -> io::Result<(vk::DeviceMemory, u32)> {
        // SAFETY: the image is this device's.
        let needs = unsafe { self.device().get_image_memory_requirements(image) };
        let kind = self
            .core
            .memory_type(needs.memory_type_bits, wanted)
            .or_else(|| {
                self.core
                    .memory_type(needs.memory_type_bits, vk::MemoryPropertyFlags::empty())
            })
            .ok_or_else(|| io::Error::other("no memory type for an image"))?;
        let mut exported = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(needs.size)
            .memory_type_index(kind)
            .push_next(&mut dedicated);
        if export {
            alloc = alloc.push_next(&mut exported);
        }
        // SAFETY: the image's own requirements, dedicated to it.
        let memory = unsafe { self.device().allocate_memory(&alloc, None) }
            .map_err(|e| said("vkAllocateMemory", e))?;
        // SAFETY: bound once, at 0.
        if let Err(e) = unsafe { self.device().bind_image_memory(image, memory, 0) } {
            // SAFETY: nothing is bound to it.
            unsafe { self.device().free_memory(memory, None) };
            return Err(said("vkBindImageMemory", e));
        }
        Ok((memory, kind))
    }

    /// The property flags of memory type `kind`.
    fn flags_of(&self, kind: u32) -> vk::MemoryPropertyFlags {
        self.memory
            .memory_types
            .get(kind as usize)
            .map_or(vk::MemoryPropertyFlags::empty(), |t| t.property_flags)
    }

    /// Write the processor's cache lines over rows `rows` of output `id`
    /// back and drop them, where the output is mapped: a card that copies
    /// the output with the processor then reads what the GPU wrote, not a
    /// line its cache kept from the frame before
    /// (`HYPRIX_GPU_OUTPUT_FLUSH=1`, for a GPU that writes exported memory
    /// past the cache).
    pub fn flush_output_lines(&self, id: u32, rows: std::ops::Range<u32>, stride: u32) {
        let Some(&(ptr, len)) = self.mapped.get(&id) else {
            return;
        };
        let start = (rows.start as usize)
            .saturating_mul(stride as usize)
            .min(len);
        let end = (rows.end as usize).saturating_mul(stride as usize).min(len);
        flush_lines(ptr, start..end);
    }

    /// A linear image of `size` the device exports as a dmabuf, for the
    /// card to be shown: drawn into only by copies from a frame.
    ///
    /// # Errors
    ///
    /// The device's, or a device with no dmabuf export.
    pub fn output(&mut self, (width, height): (u32, u32)) -> io::Result<Exported> {
        let (Some(fd_ext), Some(drm)) = (self.core.dmabuf().cloned(), self.image_drm.clone())
        else {
            return Err(io::Error::other("the device exports no dmabufs"));
        };
        let modifiers = [LINEAR];
        let ty = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(ty);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: width.max(1),
                height: height.max(1),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list)
            .push_next(&mut external);
        // SAFETY: a linear 2D image for export, as the extensions allow.
        let image = unsafe { self.device().create_image(&info, None) }
            .map_err(|e| said("vkCreateImage (output)", e))?;
        let wanted = output_memory();
        let (memory, kind) = match self.bind_new(image, true, wanted) {
            Ok(made) => made,
            Err(error) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_image(image, None) };
                return Err(error);
            }
        };
        let plane = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: a DRM-modifier image's plane 0.
        let layout = unsafe { self.device().get_image_subresource_layout(image, plane) };
        let mut chosen = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: an image made with a modifier list, bound.
        let asked = unsafe { drm.get_image_drm_format_modifier_properties(image, &mut chosen) };
        let get = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(ty);
        // SAFETY: memory allocated for export as a dmabuf.
        let raw = asked
            .map_err(|e| said("vkGetImageDrmFormatModifierPropertiesEXT", e))
            .and_then(|()| {
                // SAFETY: as above.
                unsafe { fd_ext.get_memory_fd(&get) }.map_err(|e| said("vkGetMemoryFdKHR", e))
            });
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_image(image, None) };
                // SAFETY: the image is gone.
                unsafe { self.device().free_memory(memory, None) };
                return Err(error);
            }
        };
        // SAFETY: a new descriptor the call handed to us alone.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let flags = self.flags_of(kind);
        let id = self.keep(image, memory, (width, height))?;
        if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            // SAFETY: host-visible memory of the device's own, mapped whole
            // once; the image is never mapped elsewhere in this process.
            if let Ok(ptr) = unsafe {
                self.device()
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            } {
                // SAFETY: the image is this device's.
                let size = unsafe { self.device().get_image_memory_requirements(image) }.size;
                let _ = self
                    .mapped
                    .insert(id, (ptr.cast::<u8>(), usize::try_from(size).unwrap_or(0)));
            }
        }
        Ok(Exported {
            memory: flags,
            id,
            fd,
            stride: u32::try_from(layout.row_pitch).unwrap_or(u32::MAX),
            offset: u32::try_from(layout.offset).unwrap_or(u32::MAX),
            modifier: chosen.drm_format_modifier,
        })
    }

    /// The modifiers a client's `FORMAT` dmabuf can be sampled with, and
    /// whether each is external-only (never, for a sampled image here).
    #[must_use]
    pub fn sampled_modifiers(&self) -> Vec<(u64, bool)> {
        if self.core.dmabuf().is_none() {
            return Vec::new();
        }
        let instance = self.core.instance();
        let physical = self.core.physical();
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: the physical device is the instance's; the chain lives.
        unsafe {
            instance.get_physical_device_format_properties2(physical, FORMAT, &mut props);
        }
        let count = usize::try_from(list.drm_format_modifier_count).unwrap_or(0);
        let mut found = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut found);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: as above, with room for every modifier.
        unsafe {
            instance.get_physical_device_format_properties2(physical, FORMAT, &mut props);
        }
        found
            .iter()
            .filter(|each| {
                each.drm_format_modifier_plane_count == 1
                    && each
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|each| (each.drm_format_modifier, false))
            .collect()
    }

    /// An output: a linear dmabuf where the device exports them, else a
    /// memfd ([`Vulkan::host_output`]) -- lavapipe on Ferrix.
    ///
    /// # Errors
    ///
    /// Neither could be made.
    pub fn any_output(&mut self, size: (u32, u32)) -> io::Result<Exported> {
        // `HYPRIX_GPU_OUTPUT=memfd`: memfds even where dmabufs could be had,
        // to try a copying card's path on a host that has both.
        if std::env::var_os("HYPRIX_GPU_OUTPUT").is_some_and(|value| value == "memfd") {
            return self.host_output(size);
        }
        match self.output(size) {
            Ok(output) => Ok(output),
            Err(why) => self
                .host_output(size)
                .map_err(|other| io::Error::other(format!("{why}; and no memfd output: {other}"))),
        }
    }

    /// An output in a memfd this process maps and the device writes through
    /// `VK_EXT_external_memory_host`, for a device that exports no dmabufs.
    /// Its modifier is [`MEMFD`].
    ///
    /// # Errors
    ///
    /// The device's, or a device with no host-memory import.
    pub fn host_output(&mut self, (width, height): (u32, u32)) -> io::Result<Exported> {
        let Some((host, align)) = self.core.host_import().map(|(h, a)| (h.clone(), a)) else {
            return Err(io::Error::other("the device imports no host memory"));
        };
        let stride = width.saturating_mul(4);
        let bytes = u64::from(stride) * u64::from(height);
        let align = align.max(4096);
        let len = bytes.div_ceil(align).saturating_mul(align);
        let size = usize::try_from(len).map_err(|_| io::Error::other("an output too large"))?;
        // SAFETY: a name and flags.
        let raw = unsafe { libc::memfd_create(c"hyprix-gpu-output".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: new and ours alone.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let length =
            libc::off_t::try_from(len).map_err(|_| io::Error::other("an output too large"))?;
        // SAFETY: our own memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), length) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh shared mapping of our memfd.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr.cast::<u8>();
        let unmap = || {
            // SAFETY: the mapping made above, unmapped once.
            let _ = unsafe { libc::munmap(ptr.cast(), size) };
        };
        let ty = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(ty);
        let info = vk::BufferCreateInfo::default()
            .size(len)
            .usage(vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC)
            .push_next(&mut external);
        // SAFETY: a transfer buffer for external host memory.
        let buffer = match unsafe { self.device().create_buffer(&info, None) } {
            Ok(buffer) => buffer,
            Err(e) => {
                unmap();
                return Err(said("vkCreateBuffer (host output)", e));
            }
        };
        let mut props = vk::MemoryHostPointerPropertiesEXT::default();
        // SAFETY: `ptr` is a host allocation kept mapped while the output
        // lives, of the extension's handle type.
        let asked = unsafe {
            (host.fp().get_memory_host_pointer_properties_ext)(
                self.device().handle(),
                ty,
                ptr.cast_const().cast(),
                &mut props,
            )
        };
        // SAFETY: the buffer is this device's.
        let needs = unsafe { self.device().get_buffer_memory_requirements(buffer) };
        let kind = asked.result().ok().and_then(|()| {
            self.core.memory_type(
                needs.memory_type_bits & props.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        });
        let Some(kind) = kind else {
            // SAFETY: unused.
            unsafe { self.device().destroy_buffer(buffer, None) };
            unmap();
            return Err(io::Error::other("no memory type takes the output's memfd"));
        };
        let mut from = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(ty)
            .host_pointer(ptr.cast());
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(len)
            .memory_type_index(kind)
            .push_next(&mut from);
        // SAFETY: `ptr` is mapped for `len` bytes, aligned as the device
        // asks, and stays mapped until the output is let go.
        let memory = match unsafe { self.device().allocate_memory(&alloc, None) } {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_buffer(buffer, None) };
                unmap();
                return Err(said("vkAllocateMemory (host output)", e));
            }
        };
        // SAFETY: bound once, at 0, to memory as large as the buffer.
        if let Err(e) = unsafe { self.device().bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: unused.
            unsafe { self.device().destroy_buffer(buffer, None) };
            // SAFETY: nothing bound to it.
            unsafe { self.device().free_memory(memory, None) };
            unmap();
            return Err(said("vkBindBufferMemory (host output)", e));
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let _ = self.host_outputs.insert(
            id,
            HostOutput {
                buffer,
                memory,
                ptr,
                len: size,
                stride,
                width,
                height,
            },
        );
        Ok(Exported {
            id,
            fd,
            stride,
            offset: 0,
            modifier: MEMFD,
            memory: vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        })
    }

    /// Copy `rects` of image `from` into the same places of host output
    /// `to`.
    fn copy_to_host(&mut self, from: u32, to: u32, rects: &[Region]) -> io::Result<()> {
        let (Some(src), Some(dst)) = (
            self.images.get(&from).map(|i| (i.image, i.width, i.height)),
            self.host_outputs
                .get(&to)
                .map(|o| (o.buffer, o.stride, o.width, o.height)),
        ) else {
            return Err(io::Error::other("a copy between unknown images"));
        };
        let regions: Vec<vk::BufferImageCopy> = rects
            .iter()
            .filter_map(|r| {
                let width = r.width.min(src.1.min(dst.2).saturating_sub(r.x));
                let height = r.height.min(src.2.min(dst.3).saturating_sub(r.y));
                (width > 0 && height > 0).then(|| vk::BufferImageCopy {
                    buffer_offset: u64::from(r.y) * u64::from(dst.1) + u64::from(r.x) * 4,
                    buffer_row_length: dst.1 / 4,
                    buffer_image_height: 0,
                    image_subresource: color_layers(),
                    image_offset: vk::Offset3D {
                        x: i32::try_from(r.x).unwrap_or(0),
                        y: i32::try_from(r.y).unwrap_or(0),
                        z: 0,
                    },
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                })
            })
            .collect();
        if regions.is_empty() {
            return Ok(());
        }
        self.ready_to_copy()?;
        // SAFETY: recording outside a rendering; the image is GENERAL and
        // every region inside it and inside the buffer.
        unsafe {
            self.device().cmd_copy_image_to_buffer(
                self.cmd,
                src.0,
                vk::ImageLayout::GENERAL,
                dst.0,
                &regions,
            );
        }
        Ok(())
    }

    /// Let host output `id` go. The device must be done with it.
    fn destroy_host_output(&self, output: HostOutput) {
        // SAFETY: nothing pending uses it.
        unsafe { self.device().destroy_buffer(output.buffer, None) };
        // SAFETY: the buffer bound to it is gone.
        unsafe { self.device().free_memory(output.memory, None) };
        // SAFETY: the mapping made in `host_output`, unmapped once, after
        // the device's import of it is gone.
        let _ = unsafe { libc::munmap(output.ptr.cast(), output.len) };
    }

    /// Copy `rects` of image `from` into the same places of image `to`.
    ///
    /// # Errors
    ///
    /// The device's; an unknown image.
    pub fn copy(&mut self, from: u32, to: u32, rects: &[Region]) -> io::Result<()> {
        if self.host_outputs.contains_key(&to) {
            return self.copy_to_host(from, to, rects);
        }
        let (Some(src), Some(dst)) = (
            self.images.get(&from).map(|i| (i.image, i.width, i.height)),
            self.images.get(&to).map(|i| (i.image, i.width, i.height)),
        ) else {
            return Err(io::Error::other("a copy between unknown images"));
        };
        let regions: Vec<vk::ImageCopy> = rects
            .iter()
            .filter_map(|r| {
                let width = r.width.min(src.1.min(dst.1).saturating_sub(r.x));
                let height = r.height.min(src.2.min(dst.2).saturating_sub(r.y));
                (width > 0 && height > 0).then(|| {
                    let offset = vk::Offset3D {
                        x: i32::try_from(r.x).unwrap_or(0),
                        y: i32::try_from(r.y).unwrap_or(0),
                        z: 0,
                    };
                    vk::ImageCopy {
                        src_subresource: color_layers(),
                        src_offset: offset,
                        dst_subresource: color_layers(),
                        dst_offset: offset,
                        extent: vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        },
                    }
                })
            })
            .collect();
        if regions.is_empty() {
            return Ok(());
        }
        self.ready_to_copy()?;
        // SAFETY: recording outside a rendering; both images GENERAL, the
        // regions inside both.
        unsafe {
            self.device().cmd_copy_image(
                self.cmd,
                src.0,
                vk::ImageLayout::GENERAL,
                dst.0,
                vk::ImageLayout::GENERAL,
                &regions,
            );
        }
        Ok(())
    }

    /// The set sampling `id` as asked, made the first time.
    fn set(&mut self, id: u32, opaque: bool, filter: Filter) -> Option<vk::DescriptorSet> {
        let (o, f) = (usize::from(opaque), usize::from(filter == Filter::Linear));
        let image = self.images.get(&id)?;
        let held = *image.sets.get(o)?.get(f)?;
        if held != vk::DescriptorSet::null() {
            return Some(held);
        }
        let view = *image.views.get(o)?;
        let sampler = *self.samplers.get(f)?;
        let layouts = [self.set_layout];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptors)
            .set_layouts(&layouts);
        // SAFETY: the pool and layout are this device's.
        let set = unsafe { self.device().allocate_descriptor_sets(&alloc) }
            .ok()?
            .first()
            .copied()?;
        let infos = [vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let write = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&infos)];
        // SAFETY: the set is new and unused.
        unsafe { self.device().update_descriptor_sets(&write, &[]) };
        if let Some(slot) = self
            .images
            .get_mut(&id)
            .and_then(|image| image.sets.get_mut(o))
            .and_then(|row| row.get_mut(f))
        {
            *slot = set;
        }
        Some(set)
    }

    /// Open a rendering into the target, if none is open.
    fn open(&mut self) -> io::Result<()> {
        if self.rendering {
            return Ok(());
        }
        let Some(target) = self.target else {
            return Err(io::Error::other("a draw with nothing aimed at"));
        };
        let Some((image, width, height)) = self
            .images
            .get(&target)
            .map(|i| (i.views[0], i.width, i.height))
        else {
            return Err(io::Error::other("a draw into an unknown image"));
        };
        self.record()?;
        self.barrier();
        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(image)
            .image_layout(vk::ImageLayout::GENERAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        };
        let info = vk::RenderingInfo::default()
            .render_area(area)
            .layer_count(1)
            .color_attachments(&attachments);
        let device = self.core.device();
        // SAFETY: recording, outside a rendering; the view is the target's,
        // in GENERAL.
        unsafe { device.cmd_begin_rendering(self.cmd, &info) };
        #[expect(clippy::cast_precision_loss, reason = "a size at most 16384")]
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        // SAFETY: inside the rendering just begun.
        unsafe { device.cmd_set_viewport(self.cmd, 0, &viewport) };
        // SAFETY: as above.
        unsafe { device.cmd_set_scissor(self.cmd, 0, &[area]) };
        #[expect(clippy::cast_precision_loss, reason = "a size at most 16384")]
        let to_clip = [2.0 / width as f32, 2.0 / height as f32, -1.0, -1.0];
        let bytes: Vec<u8> = to_clip.iter().flat_map(|v| v.to_le_bytes()).collect();
        // SAFETY: 16 bytes at 0, inside the layout's range.
        unsafe {
            device.cmd_push_constants(
                self.cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                &bytes,
            );
        }
        self.rendering = true;
        Ok(())
    }

    /// [`Backend::draw`], with its errors.
    fn try_draw(&mut self, vertices: &[[f32; 4]]) -> io::Result<()> {
        if vertices.is_empty() || self.lost {
            return Ok(());
        }
        let Some((program, blend)) = self.program else {
            return Err(io::Error::other("a draw with no program"));
        };
        let pipeline = self.pipeline(program, blend)?;
        let bytes: Vec<u8> = vertices
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.record()?;
        self.room(Which::Vertices, len)?;
        let set = if program.samples() {
            let Some((id, opaque, filter)) = self.sampled else {
                return Err(io::Error::other("a draw that samples nothing"));
            };
            Some(
                self.set(id, opaque, filter)
                    .ok_or_else(|| io::Error::other("no descriptor set"))?,
            )
        } else {
            None
        };
        self.open()?;
        let Some(buffer) = &self.vertices else {
            return Err(io::Error::other("no vertex buffer"));
        };
        let at = self.vertex_at;
        // SAFETY: the buffer is mapped for `size` bytes and `room` made
        // `at + len` fit.
        let into = unsafe { buffer.ptr.add(usize::try_from(at).unwrap_or(0)) };
        // SAFETY: `into` has room for the bytes; nothing pending reads it.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), into, bytes.len()) };
        let vertex_buffer = buffer.buffer;
        let constants: Vec<u8> = self
            .constants
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let device = self.core.device();
        // SAFETY: inside a rendering, with a pipeline made for its format.
        unsafe {
            device.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
        }
        if let Some(set) = set {
            // SAFETY: a set of the layout's one set layout.
            unsafe {
                device.cmd_bind_descriptor_sets(
                    self.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[set],
                    &[],
                );
            }
        }
        // SAFETY: 64 bytes at 16, inside the layout's 80.
        unsafe {
            device.cmd_push_constants(
                self.cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                16,
                &constants,
            );
        }
        // SAFETY: the buffer holds the vertices at `at`.
        unsafe { device.cmd_bind_vertex_buffers(self.cmd, 0, &[vertex_buffer], &[at]) };
        let count = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
        // SAFETY: inside the rendering, everything the pipeline reads bound.
        unsafe { device.cmd_draw(self.cmd, count, 1, 0, 0) };
        self.vertex_at = at.saturating_add(len);
        Ok(())
    }

    /// [`Backend::upload`], with the recording begun.
    fn try_upload(&mut self, id: u32, region: Region, stride: u32, data: &[u8]) -> io::Result<()> {
        let Some(image) = self.images.get(&id).map(|i| i.image) else {
            return Err(io::Error::other("an upload into an unknown image"));
        };
        let row = region.width as usize * 4;
        let rows = region.height as usize;
        if row == 0 || rows == 0 {
            return Ok(());
        }
        let needed = (rows - 1)
            .saturating_mul(stride as usize)
            .saturating_add(row);
        if data.len() < needed {
            return Err(io::Error::other("an upload shorter than its region"));
        }
        let len = u64::try_from(row * rows).unwrap_or(u64::MAX);
        self.record()?;
        self.room(Which::Staging, len)?;
        self.ready_to_copy()?;
        let Some(buffer) = &self.staging else {
            return Err(io::Error::other("no staging buffer"));
        };
        let at = self.staging_at;
        for index in 0..rows {
            let Some(from) = data.get(index * stride as usize..index * stride as usize + row)
            else {
                break;
            };
            // SAFETY: mapped for `size` bytes, and `room` made `at + len`
            // fit.
            let into = unsafe {
                buffer
                    .ptr
                    .add(usize::try_from(at).unwrap_or(0) + index * row)
            };
            // SAFETY: `into` has room for the row; nothing pending reads it.
            unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), into, row) };
        }
        let copy = [vk::BufferImageCopy {
            buffer_offset: at,
            buffer_row_length: region.width,
            buffer_image_height: region.height,
            image_subresource: color_layers(),
            image_offset: vk::Offset3D {
                x: i32::try_from(region.x).unwrap_or(0),
                y: i32::try_from(region.y).unwrap_or(0),
                z: 0,
            },
            image_extent: vk::Extent3D {
                width: region.width,
                height: region.height,
                depth: 1,
            },
        }];
        let staging = buffer.buffer;
        // SAFETY: recording outside a rendering; the image is GENERAL and
        // the region inside it.
        unsafe {
            self.device().cmd_copy_buffer_to_image(
                self.cmd,
                staging,
                image,
                vk::ImageLayout::GENERAL,
                &copy,
            );
        }
        self.staging_at = at.saturating_add(len);
        Ok(())
    }

    /// [`Backend::import`], with its errors.
    fn try_import(&mut self, fd: BorrowedFd<'_>, layout: &Layout) -> io::Result<u32> {
        let Some(fd_ext) = self.core.dmabuf().cloned() else {
            return Err(io::Error::other("the device imports no dmabufs"));
        };
        let ty = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        let planes = [vk::SubresourceLayout {
            offset: u64::from(layout.offset),
            size: 0,
            row_pitch: u64::from(layout.stride),
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(layout.modifier)
            .plane_layouts(&planes);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(ty);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: layout.width.max(1),
                height: layout.height.max(1),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut explicit)
            .push_next(&mut external);
        // SAFETY: a 2D image over a dmabuf, as the extensions allow.
        let image = unsafe { self.device().create_image(&info, None) }
            .map_err(|e| said("vkCreateImage (import)", e))?;
        // The import takes the descriptor it is given: a duplicate.
        // SAFETY: `fd` is open for the call.
        let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        let mut props = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: a dmabuf's descriptor of the right handle type.
        let asked = unsafe { fd_ext.get_memory_fd_properties(ty, raw, &mut props) };
        // SAFETY: the image is this device's.
        let needs = unsafe { self.device().get_image_memory_requirements(image) };
        let kind = asked.ok().and_then(|()| {
            self.core.memory_type(
                needs.memory_type_bits & props.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        });
        let Some(kind) = kind.filter(|_| raw >= 0) else {
            if raw >= 0 {
                // SAFETY: our own duplicate, never handed on.
                drop(unsafe { OwnedFd::from_raw_fd(raw) });
            }
            // SAFETY: unused.
            unsafe { self.device().destroy_image(image, None) };
            return Err(io::Error::other("no memory type takes the dmabuf"));
        };
        let mut from = vk::ImportMemoryFdInfoKHR::default().handle_type(ty).fd(raw);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(needs.size)
            .memory_type_index(kind)
            .push_next(&mut from)
            .push_next(&mut dedicated);
        // SAFETY: the import's own requirements; on success the descriptor
        // is the driver's.
        let memory = match unsafe { self.device().allocate_memory(&alloc, None) } {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: the failed import left the descriptor ours.
                drop(unsafe { OwnedFd::from_raw_fd(raw) });
                // SAFETY: unused.
                unsafe { self.device().destroy_image(image, None) };
                return Err(said("vkAllocateMemory (import)", e));
            }
        };
        // SAFETY: bound once, at 0.
        if let Err(e) = unsafe { self.device().bind_image_memory(image, memory, 0) } {
            // SAFETY: unused.
            unsafe { self.device().destroy_image(image, None) };
            // SAFETY: the image is gone.
            unsafe { self.device().free_memory(memory, None) };
            return Err(said("vkBindImageMemory (import)", e));
        }
        self.keep(image, memory, (layout.width, layout.height))
    }

    /// [`Backend::read_into`], with the region checked.
    fn try_read(&mut self, id: u32, region: Region) -> io::Result<Vec<u8>> {
        let Some(image) = self.images.get(&id).map(|i| i.image) else {
            return Err(io::Error::other("a read of an unknown image"));
        };
        let len = u64::from(region.width) * u64::from(region.height) * 4;
        if len == 0 {
            return Ok(Vec::new());
        }
        if self.readback.as_ref().is_none_or(|b| b.size < len) {
            if let Some(old) = self.readback.take() {
                self.submit_and_wait()?;
                self.destroy_buffer(old);
            }
            self.readback = Some(self.host_buffer(len, vk::BufferUsageFlags::TRANSFER_DST)?);
        }
        self.ready_to_copy()?;
        let Some(buffer) = self.readback.as_ref().map(|b| (b.buffer, b.ptr)) else {
            return Err(io::Error::other("no read-back buffer"));
        };
        let copy = [vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: region.width,
            buffer_image_height: region.height,
            image_subresource: color_layers(),
            image_offset: vk::Offset3D {
                x: i32::try_from(region.x).unwrap_or(0),
                y: i32::try_from(region.y).unwrap_or(0),
                z: 0,
            },
            image_extent: vk::Extent3D {
                width: region.width,
                height: region.height,
                depth: 1,
            },
        }];
        // SAFETY: recording outside a rendering; GENERAL image.
        unsafe {
            self.device().cmd_copy_image_to_buffer(
                self.cmd,
                image,
                vk::ImageLayout::GENERAL,
                buffer.0,
                &copy,
            );
        }
        self.submit_and_wait()?;
        let len = usize::try_from(len).unwrap_or(0);
        // SAFETY: mapped for at least `len` bytes, written by a finished
        // submission behind a barrier.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.1.cast_const(), len) };
        Ok(bytes.to_vec())
    }
}

/// Which host buffer.
#[derive(Clone, Copy, Debug)]
enum Which {
    Vertices,
    Staging,
}

/// A shader module of `bytes`.
fn module(device: &ash::Device, bytes: &[u8]) -> io::Result<vk::ShaderModule> {
    let code = words(bytes);
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    // SAFETY: SPIR-V for Vulkan 1.3, checked by spirv-val when made.
    unsafe { device.create_shader_module(&info, None) }.map_err(|e| said("vkCreateShaderModule", e))
}

impl Backend for Vulkan {
    fn image(&mut self, width: u32, height: u32, usage: Use) -> io::Result<u32> {
        let mut flags = vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST;
        if usage.drawn {
            flags |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        }
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: width.max(1),
                height: height.max(1),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(flags)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: a plain 2D image.
        let image = unsafe { self.device().create_image(&info, None) }
            .map_err(|e| said("vkCreateImage", e))?;
        let memory = match self.bind_new(image, false, vk::MemoryPropertyFlags::DEVICE_LOCAL) {
            Ok((memory, _)) => memory,
            Err(error) => {
                // SAFETY: unused.
                unsafe { self.device().destroy_image(image, None) };
                return Err(error);
            }
        };
        self.keep(image, memory, (width, height))
    }

    fn import(&mut self, fd: BorrowedFd<'_>, layout: &Layout) -> io::Result<Option<u32>> {
        self.try_import(fd, layout).map(Some)
    }

    fn upload(&mut self, image: u32, region: Region, stride: u32, data: &[u8]) -> io::Result<()> {
        self.try_upload(image, region, stride, data)
    }

    fn release(&mut self, image: u32) -> io::Result<()> {
        let _ = self.mapped.remove(&image);
        if let Some(output) = self.host_outputs.remove(&image) {
            // The device may still be writing it: wait first.
            let _ = self.submit_and_wait();
            self.destroy_host_output(output);
        }
        if let Some(image) = self.images.remove(&image) {
            if self.recording {
                self.doomed.push(image);
            } else {
                self.destroy_image(image);
            }
        }
        if self.target == Some(image) {
            self.close();
            self.target = None;
        }
        Ok(())
    }

    fn aim(&mut self, image: u32, _width: u32, _height: u32) {
        if self.target != Some(image) {
            self.close();
            self.target = Some(image);
        }
    }

    fn program(&mut self, program: Program, blend: Blend, constants: &[f32]) {
        self.program = Some((program, blend));
        self.constants = [0.0; 16];
        for (slot, value) in self.constants.iter_mut().zip(constants) {
            *slot = *value;
        }
    }

    fn sample(&mut self, image: u32, opaque: bool, filter: Filter) {
        self.sampled = Some((image, opaque, filter));
    }

    fn draw(&mut self, vertices: &[[f32; 4]]) {
        if let Err(error) = self.try_draw(vertices) {
            self.fail(error);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Err(error) = self.submit_and_wait() {
            self.fail(error);
        }
        self.failed.take().map_or(Ok(()), Err)
    }

    fn read(&mut self, image: u32, region: Region) -> io::Result<Vec<u8>> {
        self.try_read(image, region)
    }

    fn read_into(
        &mut self,
        image: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        let pixels = self.try_read(image, region)?;
        let row = region.width as usize * 4;
        if row == 0 {
            return Ok(());
        }
        for (index, from) in pixels.chunks_exact(row).enumerate() {
            let target = into
                .get_mut(index * stride..index * stride + row)
                .ok_or_else(|| io::Error::other("a buffer too short for the region"))?;
            target.copy_from_slice(from);
        }
        Ok(())
    }

    fn export(&mut self, _image: u32) -> io::Result<Option<OwnedFd>> {
        Ok(None)
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        // SAFETY: waiting on the device's own queue.
        let _ = unsafe { self.device().device_wait_idle() };
        for (_, image) in std::mem::take(&mut self.images) {
            self.destroy_image(image);
        }
        for image in std::mem::take(&mut self.doomed) {
            self.destroy_image(image);
        }
        for (_, output) in std::mem::take(&mut self.host_outputs) {
            self.destroy_host_output(output);
        }
        for buffer in [
            self.vertices.take(),
            self.staging.take(),
            self.readback.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.destroy_buffer(buffer);
        }
        let device = self.core.device();
        for (_, pipeline) in self.pipelines.drain() {
            // SAFETY: idle; nothing uses it.
            unsafe { device.destroy_pipeline(pipeline, None) };
        }
        for (_, module) in self.fragment_modules.drain() {
            // SAFETY: as above.
            unsafe { device.destroy_shader_module(module, None) };
        }
        // SAFETY: as above, for each below.
        unsafe { device.destroy_shader_module(self.vertex_module, None) };
        for sampler in self.samplers {
            // SAFETY: as above.
            unsafe { device.destroy_sampler(sampler, None) };
        }
        // SAFETY: as above; destroying the pool frees its sets.
        unsafe { device.destroy_descriptor_pool(self.descriptors, None) };
        // SAFETY: as above.
        unsafe { device.destroy_pipeline_layout(self.pipeline_layout, None) };
        // SAFETY: as above.
        unsafe { device.destroy_descriptor_set_layout(self.set_layout, None) };
        // SAFETY: as above.
        unsafe { device.destroy_fence(self.fence, None) };
        // SAFETY: as above; the pool frees its buffer.
        unsafe { device.destroy_command_pool(self.pool, None) };
    }
}

/// The memory an output is made in: `HYPRIX_GPU_OUTPUT_MEMORY` says
/// `device` (device-local), `host` (host-visible and coherent), `cached`
/// (host-visible and cached), or nothing (the driver's first type).
fn output_memory() -> vk::MemoryPropertyFlags {
    match std::env::var("HYPRIX_GPU_OUTPUT_MEMORY").as_deref() {
        Ok("device") => vk::MemoryPropertyFlags::DEVICE_LOCAL,
        Ok("host") => {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        }
        Ok("cached") => {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED
        }
        _ => vk::MemoryPropertyFlags::empty(),
    }
}

/// Write back and drop the processor's cache lines over `range` of the
/// mapping at `ptr`.
#[cfg(target_arch = "x86_64")]
fn flush_lines(ptr: *mut u8, range: std::ops::Range<usize>) {
    const LINE: usize = 64;
    let mut at = range.start & !(LINE - 1);
    while at < range.end {
        // SAFETY: `at` is inside the mapping (the caller's range is).
        let line = unsafe { ptr.add(at) };
        // SAFETY: a mapped line; clflush only writes it back and drops it.
        unsafe { core::arch::x86_64::_mm_clflush(line.cast_const()) };
        at += LINE;
    }
    // SAFETY: a fence, ordering the flushes before what follows.
    unsafe { core::arch::x86_64::_mm_mfence() };
}

/// The same, where there is no clflush: nothing.
#[cfg(not(target_arch = "x86_64"))]
fn flush_lines(_ptr: *mut u8, _range: std::ops::Range<usize>) {}
