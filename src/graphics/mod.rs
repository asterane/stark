// STARK, a system for computer augmented design.
// Copyright (C) 2021 Matthew Rothlisberger

// STARK is free software: you can redistribute it and / or modify it
// under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.

// STARK is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// Affero General Public License for more details.

// You should have received a copy of the GNU Affero General Public
// License along with STARK (in the LICENSE file). If not, see
// <https://www.gnu.org/licenses/>.

// Find full copyright information in the top level COPYRIGHT file.

// <>

// src/graphics/mod.rs

// Graphics rendering system for STARK. Relies on gfx-hal for low
// level access to Vulkan and other graphics APIs. Contains code for a
// dedicate rendering thread.

// <>

use crate::sail::{self, SlHead};
use crate::FrameHandle;

use gfx_hal::{
    adapter::{Adapter, PhysicalDevice},
    command::{
        ClearColor, ClearValue, CommandBuffer, CommandBufferFlags, RenderAttachmentInfo,
        SubpassContents,
    },
    device::Device,
    format::{ChannelType, Format},
    image::{Extent, Layout},
    memory::Segment,
    pass::{Attachment, AttachmentLoadOp, AttachmentOps, AttachmentStoreOp, Subpass, SubpassDesc},
    pool::CommandPool,
    pso::{
        AttributeDesc, BlendState, ColorBlendDesc, ColorMask, Element, EntryPoint,
        GraphicsPipelineDesc, InputAssemblerDesc, Primitive, PrimitiveAssemblerDesc, Rasterizer,
        Rect, ShaderStageFlags, Specialization, VertexBufferDesc, VertexInputRate, Viewport,
    },
    queue::{Queue, QueueFamily, QueueGroup},
    window::{Extent2D, PresentationSurface, Surface, SwapchainConfig},
    Instance, MemoryTypeId,
};

use std::borrow::Borrow;
use std::iter;
use std::mem::size_of;

/// Sail interpreter loop for the render thread (holds graphics state)
pub fn render_loop(
    name: &'static str,
    size: [u32; 2],
    window: &FrameHandle,
    sl_reg: usize,
    sl_tbl: usize,
    sl_env: usize,
) {
    let sl_reg = sl_reg as *mut sail::memmgt::Region;
    let sl_tbl = sl_tbl as *mut SlHead;
    let sl_env = sl_env as *mut SlHead;

    let mut engine: Engine<backend::Backend> =
        Engine::new(GraphicsState::new(window, name, size[0], size[1]));

    let eng_obj = unsafe { sail::memmgt::alloc(sl_reg, 8, sail::Cfg::B8Other as u8) };
    unsafe { sail::write_field_unchecked(eng_obj, 0, (&mut engine as *mut _) as u64) };
    sail::env_layer_ins_by_id(sl_reg, sl_env, sail::S_ENGINE.0, eng_obj);

    crate::sail_fn! {
        let rndr_fns;
        _reg _tbl _env;

        "redraw" 1 [eng_ptr] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            engine.draw_frame();

            return sail::nil();
        }

        "frame-size" 3 [eng_ptr, w, h] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            let w = sail::u32_get(w);
            let h = sail::u32_get(h);

            engine.state.set_extent(w, h);
            engine.should_configure_swapchain = true;

            return sail::nil();
        }

        "add-line" 3 [eng_ptr, points, colors] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            assert_eq!(sail::core_type(points), Some(sail::CoreType::VecArr));
            assert_eq!(sail::core_read_field::<u32>(points, 0), sail::T_F32.0);

            assert_eq!(sail::core_type(colors), Some(sail::CoreType::VecArr));
            assert_eq!(sail::core_read_field::<u32>(colors, 0), sail::T_F32.0);

            let (ln, cl) = unsafe {
                (
                    std::ptr::read_unaligned::<[f32; 4]>(sail::value_ptr(points).add(8) as *mut _),
                    std::ptr::read_unaligned::<[f32; 3]>(sail::value_ptr(colors).add(8) as *mut _)
                )
            };

            engine.add_line(ln, cl);

            return sail::nil();
        }

        "pop-line" 1 [eng_ptr] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            engine.lines.pop();
            engine.colors.pop();

            return sail::nil();
        }

        "bg-col" 4 [eng_ptr, r, g, b] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            engine.set_clear([
                sail::f32_get(r),
                sail::f32_get(g),
                sail::f32_get(b),
                1.0,
            ]);

            return sail::nil();
        }

        "clear" 1 [eng_ptr] {
            assert_eq!(sail::get_cfg_spec(eng_ptr), sail::Cfg::B8Other);
            let engine = unsafe {
                &mut *(sail::read_field_unchecked::<u64>(eng_ptr, 0) as *mut Engine<backend::Backend>)
            };

            engine.empty_lines();

            return sail::nil();
        }
    }

    sail::insert_native_procs(sl_reg, sl_tbl, sl_env, rndr_fns);

    engine.setup();
    engine.set_clear([1.0, 1.0, 1.0, 1.0]);

    let prog_txt = &std::fs::read_to_string("scripts/rndr.sl").unwrap();
    let prog_expr = sail::parser::parse(sl_reg, sl_tbl, prog_txt).unwrap();

    let mut stack = sail::eval::EvalStack::new(10000);

    let sigil = 1 as *mut SlHead;

    let mut ret_slot = sigil;
    let ret_addr: *mut *mut SlHead = &mut ret_slot;

    stack.start(ret_addr, sl_env, prog_expr);

    while ret_slot == sigil {
        stack.iter_once(sl_reg, sl_tbl);
    }

    let rndr = sail::env_lookup_by_id(sl_env, sail::S_RNDR.0);

    stack.push_frame_head(ret_addr, sail::eval::Opcode::Apply, sl_env);
    stack.push(rndr);

    loop {
        if engine.should_configure_swapchain {
            engine.state.config_swapchain();
            engine.draw_frame();
            engine.should_configure_swapchain = false;
        }

        stack.iter_once(sl_reg, sl_tbl);

        if stack.is_empty() {
            println!("render thread ended");
            break;
        }
    }

    // TODO: dispose of Sail environment first (reverse creation order)
    drop(engine);
}

/// Sail-specific graphics engine state
pub struct Engine<B: gfx_hal::Backend> {
    clear: [f32; 4],
    lines: Vec<[f32; 4]>,
    colors: Vec<[f32; 3]>,
    buflen: u64,
    state: GraphicsState<B>,
    should_configure_swapchain: bool,
}

impl<B: gfx_hal::Backend> Engine<B> {
    /// Initialize the graphics engine
    fn new(state: GraphicsState<B>) -> Self {
        Self {
            clear: [0.0, 0.0, 0.0, 1.0],
            lines: vec![],
            colors: vec![],
            buflen: 256,
            state,
            should_configure_swapchain: true,
        }
    }
    /// Set the clear (background) color
    fn set_clear(&mut self, clear: [f32; 4]) {
        self.clear = clear;
    }
    /// Add a line, with two endpoints and a color
    fn add_line(&mut self, points: [f32; 4], color: [f32; 3]) {
        self.lines.push(points);
        self.colors.push(color);
        self.buffer_size_check();
    }
    /// Empty the engine of all lines
    fn empty_lines(&mut self) {
        self.lines.clear();
        self.colors.clear();
    }
    /// Set up an appropriate graphics pipeline for the engine
    fn state_pipeline_setup(&mut self) {
        let pipeline_layout = unsafe {
            self.state
                .device
                .create_pipeline_layout(
                    iter::empty(),
                    iter::once((ShaderStageFlags::FRAGMENT, 0..12)),
                )
                .unwrap()
        };

        let vertex_shader = include_str!("shaders/lines.vert");
        let fragment_shader = include_str!("shaders/lines.frag");

        let pipeline = unsafe {
            self.state.make_pipeline(
                &pipeline_layout,
                vertex_shader,
                fragment_shader,
                Primitive::LineList,
            )
        };

        self.state.pipeline_layouts.push(pipeline_layout);
        self.state.pipelines.push(pipeline);
    }
    /// Acquire memory and create buffer for vertex data
    fn state_buffer_setup(&mut self) {
        unsafe {
            self.state
                .device
                .wait_for_fence(
                    self.state.submission_complete_fence.as_ref().unwrap(),
                    1_000_000_000,
                )
                .unwrap();

            for mem in self.state.vertex_memory.drain(..) {
                self.state.device.free_memory(mem);
            }

            for buf in self.state.vertex_buffers.drain(..) {
                self.state.device.destroy_buffer(buf);
            }
        }

        let (memory, buffer) = unsafe {
            self.state.make_buffer(
                self.buflen,
                gfx_hal::buffer::Usage::VERTEX,
                gfx_hal::memory::Properties::CPU_VISIBLE,
            )
        };

        self.state.vertex_memory.push(memory);
        self.state.vertex_buffers.push(buffer);
    }
    /// Check whether the buffer has enough space for all vertices
    fn buffer_size_check(&mut self) {
        let line_vec_size = size_of::<[f32; 4]>() * self.lines.len();

        if line_vec_size as u64 >= self.buflen {
            self.buflen = 2 * self.buflen;
            self.state_buffer_setup();
        }
    }
    /// Prepare the engine to accept draw calls
    fn setup(&mut self) {
        self.state_buffer_setup();
        self.state_pipeline_setup();
    }
    /// Draw a single frame according to the engine state
    fn draw_frame(&mut self) {
        let timeout_ns = 1_000_000_000;

        unsafe {
            self.state
                .device
                .wait_for_fence(
                    self.state.submission_complete_fence.as_ref().unwrap(),
                    timeout_ns,
                )
                .unwrap();
            self.state
                .device
                .reset_fence(self.state.submission_complete_fence.as_mut().unwrap())
                .unwrap();

            self.state.command_pool.as_mut().unwrap().reset(false);
        }

        let surface_image = unsafe {
            match self
                .state
                .surface
                .as_mut()
                .unwrap()
                .acquire_image(timeout_ns)
            {
                Ok((image, _)) => image,
                Err(gfx_hal::window::AcquireError::OutOfDate(_)) => {
                    self.should_configure_swapchain = true;
                    return;
                }
                Err(_) => {
                    return;
                }
                // TODO: these errors must be handled
                // Err(err) => panic!("{:?}", err),
            }
        };

        let line_vec_size = size_of::<[f32; 4]>() * self.lines.len();

        if line_vec_size > 0 {
            unsafe {
                let mapped_memory = self
                    .state
                    .device
                    .map_memory(&mut self.state.vertex_memory[0], Segment::ALL)
                    .unwrap();

                std::ptr::copy_nonoverlapping(
                    self.lines.as_ptr() as *const u8,
                    mapped_memory,
                    line_vec_size,
                );

                self.state
                    .device
                    .flush_mapped_memory_ranges(iter::once((
                        &self.state.vertex_memory[0],
                        Segment::ALL,
                    )))
                    .unwrap();

                self.state
                    .device
                    .unmap_memory(&mut self.state.vertex_memory[0]);
            }
        }

        unsafe {
            let buffer = &mut self.state.command_buffers[0];

            let viewport = Viewport {
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: self.state.surface_extent.width as i16,
                    h: self.state.surface_extent.height as i16,
                },
                depth: 0.0..1.0,
            };

            buffer.begin_primary(CommandBufferFlags::ONE_TIME_SUBMIT);

            buffer.set_viewports(0, iter::once(viewport.clone()));
            buffer.set_scissors(0, iter::once(viewport.rect));

            buffer.begin_render_pass(
                &self.state.render_passes[0],
                &self.state.framebuffer.as_ref().unwrap(),
                viewport.rect,
                iter::once(RenderAttachmentInfo {
                    image_view: surface_image.borrow(),
                    clear_value: ClearValue {
                        color: ClearColor {
                            float32: self.clear,
                        },
                    },
                }),
                SubpassContents::Inline,
            );

            buffer.bind_graphics_pipeline(&self.state.pipelines[0]);

            buffer.bind_vertex_buffers(
                0,
                iter::once((
                    &self.state.vertex_buffers[0],
                    gfx_hal::buffer::SubRange {
                        offset: 0,
                        size: Some(line_vec_size as u64),
                    },
                )),
            );

            for l in 0..self.lines.len() as u32 {
                buffer.push_graphics_constants(
                    &self.state.pipeline_layouts[0],
                    ShaderStageFlags::FRAGMENT,
                    0,
                    std::mem::transmute::<&[f32], &[u32]>(&self.colors[l as usize][..]),
                );

                let ind = 2 * l;
                buffer.draw(ind..(ind + 2), 0..1);
            }

            buffer.end_render_pass();
            buffer.finish();
        }

        unsafe {
            self.state.queue_group.queues[0].submit(
                iter::once(&self.state.command_buffers[0]),
                iter::empty(),
                iter::once(self.state.rendering_complete_semaphore.as_ref().unwrap()),
                self.state.submission_complete_fence.as_mut(),
            );

            // TODO: handle possible errors here
            match self.state.queue_group.queues[0].present(
                self.state.surface.as_mut().unwrap(),
                surface_image,
                self.state.rendering_complete_semaphore.as_mut(),
            ) {
                Ok(_) => (),
                Err(gfx_hal::window::PresentError::OutOfDate(_)) => {
                    self.should_configure_swapchain = true;
                    return;
                }
                Err(err) => panic!("{:?}", err),
            }
        }
    }
}

/// Stores all persistent `gfx-hal` (Vulkan) objects
pub struct GraphicsState<B: gfx_hal::Backend> {
    surface_extent: Extent2D,
    instance: B::Instance,
    surface: Option<B::Surface>,
    adapter: Adapter<B>,
    device: B::Device,
    queue_group: QueueGroup<B>,
    surface_color_format: Format,
    render_passes: Vec<B::RenderPass>,
    framebuffer: Option<B::Framebuffer>,
    command_pool: Option<B::CommandPool>,
    command_buffers: Vec<B::CommandBuffer>,
    vertex_memory: Vec<B::Memory>,
    vertex_buffers: Vec<B::Buffer>,
    pipeline_layouts: Vec<B::PipelineLayout>,
    pipelines: Vec<B::GraphicsPipeline>,
    submission_complete_fence: Option<B::Fence>,
    rendering_complete_semaphore: Option<B::Semaphore>,
}

impl<B: gfx_hal::Backend> GraphicsState<B> {
    /// Initialize the graphics system and track necessary state
    pub fn new(window: &FrameHandle, name: &str, width: u32, height: u32) -> Self {
        let surface_extent = Extent2D { width, height };
        let instance = B::Instance::create(name, 1).unwrap();
        let surface = unsafe { instance.create_surface(window).unwrap() };
        let adapter = instance
            .enumerate_adapters()
            .into_iter()
            .find(|a| {
                a.queue_families.iter().any(|qf| {
                    surface.supports_queue_family(qf) && qf.queue_type().supports_graphics()
                })
            })
            .unwrap();
        let queue_family = adapter
            .queue_families
            .iter()
            .find(|qf| surface.supports_queue_family(qf) && qf.queue_type().supports_graphics())
            .unwrap();
        let mut gpu = unsafe {
            adapter
                .physical_device
                .open(&[(queue_family, &[1.0])], gfx_hal::Features::empty())
                .unwrap()
        };
        let device = gpu.device;
        let queue_group = gpu.queue_groups.remove(queue_family.id().0);
        let supported_formats = surface
            .supported_formats(&adapter.physical_device)
            .unwrap_or(vec![]);
        let default_format = *supported_formats.get(0).unwrap_or(&Format::Rgba8Srgb);
        let surface_color_format = supported_formats
            .into_iter()
            .find(|f| f.base_format().1 == ChannelType::Srgb)
            .unwrap_or(default_format);
        let color_attachment = Attachment {
            format: Some(surface_color_format),
            samples: 1,
            ops: AttachmentOps::new(AttachmentLoadOp::Clear, AttachmentStoreOp::Store),
            stencil_ops: AttachmentOps::DONT_CARE,
            layouts: Layout::Undefined..Layout::Present,
        };
        let subpass = SubpassDesc {
            colors: &[(0, Layout::ColorAttachmentOptimal)],
            depth_stencil: None,
            inputs: &[],
            resolves: &[],
            preserves: &[],
        };
        let render_pass = unsafe {
            device
                .create_render_pass(
                    vec![color_attachment].into_iter(),
                    vec![subpass].into_iter(),
                    vec![].into_iter(),
                )
                .unwrap()
        };
        let mut command_pool = unsafe {
            device
                .create_command_pool(
                    queue_group.family,
                    gfx_hal::pool::CommandPoolCreateFlags::empty(),
                )
                .unwrap()
        };
        let command_buffer = unsafe { command_pool.allocate_one(gfx_hal::command::Level::Primary) };

        let submission_complete_fence = device.create_fence(false).unwrap();
        let rendering_complete_semaphore = device.create_semaphore().unwrap();

        Self {
            surface_extent,
            instance,
            surface: Some(surface),
            adapter,
            device,
            framebuffer: None,
            queue_group,
            surface_color_format,
            render_passes: vec![render_pass],
            command_pool: Some(command_pool),
            command_buffers: vec![command_buffer],
            vertex_buffers: vec![],
            vertex_memory: vec![],
            pipeline_layouts: vec![],
            pipelines: vec![],
            submission_complete_fence: Some(submission_complete_fence),
            rendering_complete_semaphore: Some(rendering_complete_semaphore),
        }
    }

    /// Draw a frame that is cleared to the specified color
    pub fn draw_clear_frame(&mut self, color: [f32; 4]) -> Result<(), &str> {
        let timeout_ns = 1_000_000_000;

        unsafe {
            self.device
                .wait_for_fence(self.submission_complete_fence.as_ref().unwrap(), timeout_ns)
                .unwrap();
            self.device
                .reset_fence(self.submission_complete_fence.as_mut().unwrap())
                .unwrap();

            self.command_pool.as_mut().unwrap().reset(false);
        }

        let surface_image = unsafe {
            match self.surface.as_mut().unwrap().acquire_image(timeout_ns) {
                Ok((image, _)) => image,
                Err(_) => return Err("Could not acquire image"),
            }
        };

        unsafe {
            let buffer = &mut self.command_buffers[0];

            buffer.begin_primary(CommandBufferFlags::ONE_TIME_SUBMIT);

            buffer.begin_render_pass(
                &self.render_passes[0],
                &self.framebuffer.as_ref().unwrap(),
                Rect {
                    x: 0,
                    y: 0,
                    w: self.surface_extent.width as i16,
                    h: self.surface_extent.height as i16,
                },
                iter::once(RenderAttachmentInfo {
                    image_view: surface_image.borrow(),
                    clear_value: ClearValue {
                        color: ClearColor { float32: color },
                    },
                }),
                SubpassContents::Inline,
            );

            buffer.end_render_pass();
            buffer.finish();
        }

        unsafe {
            self.queue_group.queues[0].submit(
                vec![&self.command_buffers[0]].into_iter(),
                vec![].into_iter(),
                vec![self.rendering_complete_semaphore.as_ref().unwrap()].into_iter(),
                self.submission_complete_fence.as_mut(),
            );

            self.queue_group.queues[0]
                .present(
                    self.surface.as_mut().unwrap(),
                    surface_image,
                    self.rendering_complete_semaphore.as_mut(),
                )
                .unwrap();
        }

        Ok(())
    }

    /// Reconfigure the swapchain (usually due to new surface extent)
    pub fn config_swapchain(&mut self) {
        let caps = self
            .surface
            .as_ref()
            .unwrap()
            .capabilities(&self.adapter.physical_device);
        let swapchain_config =
            SwapchainConfig::from_caps(&caps, self.surface_color_format, self.surface_extent);
        self.surface_extent = swapchain_config.extent;

        let framebuffer = unsafe {
            if self.framebuffer.is_some() {
                self.device
                    .wait_for_fence(
                        self.submission_complete_fence.as_ref().unwrap(),
                        1_000_000_000,
                    )
                    .unwrap();

                self.device
                    .destroy_framebuffer(self.framebuffer.take().unwrap());
            }

            let framebuffer_attachment = swapchain_config.framebuffer_attachment();

            self.device
                .create_framebuffer(
                    &self.render_passes[0],
                    iter::once(framebuffer_attachment),
                    Extent {
                        width: self.surface_extent.width,
                        height: self.surface_extent.height,
                        depth: 1,
                    },
                )
                .unwrap()
        };

        self.framebuffer = Some(framebuffer);

        unsafe {
            self.surface
                .as_mut()
                .unwrap()
                .configure_swapchain(&self.device, swapchain_config)
                .unwrap();
        }
    }

    /// Set the size of the 2D graphics surface, in pixels
    pub fn set_extent(&mut self, width: u32, height: u32) {
        self.surface_extent = Extent2D { width, height };
    }

    /// Create a new buffer for graphics processing and bind its memory
    unsafe fn make_buffer(
        &mut self,
        buffer_len: u64,
        usage: gfx_hal::buffer::Usage,
        properties: gfx_hal::memory::Properties,
    ) -> (B::Memory, B::Buffer) {
        let mut buffer = self
            .device
            .create_buffer(buffer_len, usage, gfx_hal::memory::SparseFlags::empty())
            .unwrap();

        let req = self.device.get_buffer_requirements(&buffer);

        let memory_types = self
            .adapter
            .physical_device
            .memory_properties()
            .memory_types;

        let memory_type = memory_types
            .iter()
            .enumerate()
            .find(|(id, mem_type)| {
                let type_supported = req.type_mask & (1_u32 << id) != 0;
                type_supported && mem_type.properties.contains(properties)
            })
            .map(|(id, _ty)| MemoryTypeId(id))
            .unwrap();

        // log::debug!("Buffer size: {}", req.size);

        let buffer_memory = self.device.allocate_memory(memory_type, req.size).unwrap();

        self.device
            .bind_buffer_memory(&buffer_memory, 0, &mut buffer)
            .unwrap();

        (buffer_memory, buffer)
    }

    /// Compile GLSL shader code into SPIR-V
    fn compile_shader(glsl: &str, shader_kind: shaderc::ShaderKind) -> Vec<u32> {
        let mut compiler = shaderc::Compiler::new().unwrap();

        let compiled_shader = compiler
            .compile_into_spirv(glsl, shader_kind, "unnamed", "main", None)
            .expect("Failed to compile shader");

        compiled_shader.as_binary().to_vec()
    }

    /// Generate a basic graphics pipeline
    unsafe fn make_pipeline(
        &mut self,
        pipeline_layout: &B::PipelineLayout,
        vertex_shader: &str,
        fragment_shader: &str,
        primitive_type: Primitive,
    ) -> B::GraphicsPipeline {
        let vertex_shader_module = self
            .device
            .create_shader_module(&Self::compile_shader(
                vertex_shader,
                shaderc::ShaderKind::Vertex,
            ))
            .expect("Failed to create vertex shader module");

        let fragment_shader_module = self
            .device
            .create_shader_module(&Self::compile_shader(
                fragment_shader,
                shaderc::ShaderKind::Fragment,
            ))
            .expect("Failed to create fragment shader module");

        let (vs_entry, fs_entry) = (
            EntryPoint {
                entry: "main",
                module: &vertex_shader_module,
                specialization: Specialization::default(),
            },
            EntryPoint {
                entry: "main",
                module: &fragment_shader_module,
                specialization: Specialization::default(),
            },
        );

        let primitive_assembler = PrimitiveAssemblerDesc::Vertex {
            buffers: &[VertexBufferDesc {
                binding: 0,
                stride: (size_of::<f32>() * 2) as u32,
                rate: VertexInputRate::Vertex,
            }],
            attributes: &[AttributeDesc {
                location: 0,
                binding: 0,
                element: Element {
                    format: Format::Rg32Sfloat,
                    offset: 0,
                },
            }],
            input_assembler: InputAssemblerDesc::new(primitive_type),
            vertex: vs_entry,
            tessellation: None,
            geometry: None,
        };

        let mut pipeline_desc = GraphicsPipelineDesc::new(
            primitive_assembler,
            Rasterizer {
                // cull_face: Face::BACK,
                ..Rasterizer::FILL
            },
            Some(fs_entry),
            pipeline_layout,
            Subpass {
                index: 0,
                main_pass: &self.render_passes[0],
            },
        );

        pipeline_desc.blender.targets.push(ColorBlendDesc {
            mask: ColorMask::ALL,
            blend: Some(BlendState::ALPHA),
        });

        let pipeline = self
            .device
            .create_graphics_pipeline(&pipeline_desc, None)
            .expect("Failed to create graphics pipeline");

        self.device.destroy_shader_module(vertex_shader_module);
        self.device.destroy_shader_module(fragment_shader_module);

        pipeline
    }
}

impl<B: gfx_hal::Backend> Drop for GraphicsState<B> {
    fn drop(&mut self) {
        unsafe {
            self.device
                .wait_for_fence(
                    self.submission_complete_fence.as_ref().unwrap(),
                    1_000_000_000,
                )
                .unwrap();

            self.device
                .destroy_semaphore(self.rendering_complete_semaphore.take().unwrap());
            self.device
                .destroy_fence(self.submission_complete_fence.take().unwrap());

            self.device
                .destroy_framebuffer(self.framebuffer.take().unwrap());

            for render_pass in self.render_passes.drain(..) {
                self.device.destroy_render_pass(render_pass);
            }
            for mem in self.vertex_memory.drain(..) {
                self.device.free_memory(mem);
            }
            for buf in self.vertex_buffers.drain(..) {
                self.device.destroy_buffer(buf);
            }
            for pipeline in self.pipelines.drain(..) {
                self.device.destroy_graphics_pipeline(pipeline);
            }
            for pipeline_layout in self.pipeline_layouts.drain(..) {
                self.device.destroy_pipeline_layout(pipeline_layout);
            }

            self.device
                .destroy_command_pool(self.command_pool.take().unwrap());
            self.surface
                .as_mut()
                .unwrap()
                .unconfigure_swapchain(&self.device);
            self.instance.destroy_surface(self.surface.take().unwrap());
        }
    }
}
