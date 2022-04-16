// Copyright 2020-2022 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <https://opensource.org/licenses/MIT>.

//! Rendering via the [`wgpu`] WebGPU-in-Rust graphics library.
//!
//! This code is experimental and not feature-complete.

use std::borrow::Cow;
use std::sync::Arc;

use all_is_cubes::drawing::embedded_graphics::{
    mono_font::{iso_8859_1::FONT_7X13_BOLD, MonoTextStyle},
    pixelcolor::Rgb888,
    prelude::Point,
    text::{Baseline, Text},
    Drawable,
};
use instant::Instant;

use all_is_cubes::apps::{Layers, StandardCameras};
use all_is_cubes::camera::Viewport;
use all_is_cubes::cgmath::Vector2;
use all_is_cubes::character::Cursor;
use all_is_cubes::listen::DirtyFlag;
use once_cell::sync::Lazy;

use crate::{
    in_wgpu::{glue::BeltWritingParts, space::BlockRenderStuff, vertex::WgpuBlockVertex},
    reloadable::{reloadable_str, Reloadable},
};
use crate::{GraphicsResourceError, RenderInfo, SpaceRenderInfo};

mod camera;

mod block_texture;

mod frame_texture;
use frame_texture::DrawableTexture;

mod glue;

mod space;
use space::SpaceRenderer;

mod vertex;

pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Entry point for [`wgpu`] rendering. Construct this and hand it the [`wgpu::Surface`]
/// to draw on.
//#[derive(Debug)]
#[allow(missing_debug_implementations)] // TODO: wgpu::util::StagingBelt isn't Debug (will be in the future)
pub struct SurfaceRenderer {
    surface: wgpu::Surface,
    device: Arc<wgpu::Device>,
    queue: wgpu::Queue,

    everything: EverythingRenderer,

    depth_texture: wgpu::Texture,
    depth_texture_view: wgpu::TextureView,
}

impl SurfaceRenderer {
    pub async fn new(
        cameras: StandardCameras,
        surface: wgpu::Surface,
        adapter: &wgpu::Adapter,
    ) -> Result<Self, GraphicsResourceError> {
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    features: wgpu::Features::empty(),
                    limits: wgpu::Limits::downlevel_webgl2_defaults()
                        .using_resolution(wgpu::Limits::default()),
                    label: None,
                },
                None,
            )
            .await
            .unwrap();
        let device = Arc::new(device);

        let everything = EverythingRenderer::new(
            device.clone(),
            cameras,
            surface.get_preferred_format(adapter).unwrap(),
        );

        let depth_texture = create_depth_texture(&device, &everything);

        let mut new_self = Self {
            depth_texture_view: depth_texture.create_view(&Default::default()),
            depth_texture,
            everything,
            surface,
            device,
            queue,
        };
        new_self.set_viewport(new_self.viewport())?;
        Ok(new_self)
    }

    pub fn device(&self) -> &Arc<wgpu::Device> {
        &self.device
    }

    /// Returns the last [`Viewport`] provided.
    pub fn viewport(&self) -> Viewport {
        self.everything.viewport()
    }

    /// Sets the expected viewport dimensions to use for the next frame.
    pub fn set_viewport(&mut self, viewport: Viewport) -> Result<(), GraphicsResourceError> {
        log::trace!(
            "SurfaceRenderer::set_viewport {:?}",
            viewport.framebuffer_size
        );
        self.everything.set_viewport(viewport)?;

        // Test because wgpu insists on nonzero values -- we'd rather be inconsistent
        // than crash.
        let config = &self.everything.config;
        if config.width > 0 && config.height > 0 {
            self.surface.configure(&self.device, config);
            self.depth_texture = create_depth_texture(&self.device, &self.everything);
            self.depth_texture_view = self.depth_texture.create_view(&Default::default());
        }

        Ok(())
    }

    /// Sync camera to character state. This is used so that cursor raycasts can be up-to-date
    /// to the same frame of input.
    ///
    /// TODO: This is a kludge which ought to be replaced with some architecture that
    /// doesn't require a very specific "do this before this"...
    #[doc(hidden)]
    pub fn update_world_camera(&mut self) {
        self.everything.cameras.update();
    }

    pub fn cameras(&self) -> &StandardCameras {
        &self.everything.cameras
    }

    pub async fn render_frame(
        &mut self,
        cursor_result: &Option<Cursor>,
        info_text_fn: impl FnOnce(&RenderInfo) -> String,
    ) -> Result<RenderInfo, GraphicsResourceError> {
        let output = self.surface.get_current_texture()?;
        let info = self
            .everything
            .render_frame(
                cursor_result,
                &self.queue,
                &output,
                &self.depth_texture_view,
            )
            .await?;
        self.everything
            .add_info_text(&self.queue, &output, &info_text_fn(&info));
        output.present();
        Ok(info)
    }
}

/// All the state, both CPU and GPU-side, that is needed for drawing a complete
/// scene and UI, but not the surface it's drawn on. This may be used in tests or
/// to support
//#[derive(Debug)]
#[allow(missing_debug_implementations)] // TODO: wgpu::util::StagingBelt isn't Debug (will be in the future)
pub struct EverythingRenderer {
    device: Arc<wgpu::Device>,

    staging_belt: wgpu::util::StagingBelt,

    cameras: StandardCameras,

    /// Surface configuration maintained to match the viewport.
    config: wgpu::SurfaceConfiguration,

    block_render_stuff: BlockRenderStuff,
    space_renderers: Layers<Option<SpaceRenderer>>,

    // TODO: shader for debug lines
    /// Debug overlay text is uploaded via this texture
    info_text_texture: DrawableTexture,

    /// Pipeline for the info text layer.
    /// This may eventually turn into a more general post-processing render pass.
    info_text_render_pipeline: wgpu::RenderPipeline,
    info_text_bind_group: Option<wgpu::BindGroup>,
    info_text_bind_group_layout: wgpu::BindGroupLayout,
    info_text_sampler: wgpu::Sampler,
    info_text_shader_dirty: DirtyFlag,
}

impl EverythingRenderer {
    pub fn new(
        device: Arc<wgpu::Device>,
        cameras: StandardCameras,
        format: wgpu::TextureFormat,
    ) -> Self {
        let viewport = cameras.viewport();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: viewport.framebuffer_size.x,
            height: viewport.framebuffer_size.y,
            present_mode: wgpu::PresentMode::Fifo,
        };

        let info_text_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
                label: Some("EverythingRenderer::info_text_bind_group_layout"),
            });

        EverythingRenderer {
            staging_belt: wgpu::util::StagingBelt::new(
                // TODO: wild guess at good size
                std::mem::size_of::<WgpuBlockVertex>() as wgpu::BufferAddress * 4096,
            ),
            block_render_stuff: BlockRenderStuff::new(&device, config.format),
            space_renderers: Default::default(),

            info_text_shader_dirty: {
                // TODO: this is a common pattern which should get a helper method
                let flag = DirtyFlag::new(false);
                INFO_TEXT_SHADER.as_source().listen(flag.listener());
                flag
            },
            info_text_texture: DrawableTexture::new(),
            info_text_bind_group: None,
            info_text_render_pipeline: Self::create_info_text_pipeline(
                &device,
                &info_text_bind_group_layout,
                config.format,
            ),
            info_text_bind_group_layout,
            info_text_sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            }),

            device,
            config,
            cameras,
        }
    }

    /// Read info text shader and create the info text render pipeline.
    fn create_info_text_pipeline(
        device: &wgpu::Device,
        info_text_bind_group_layout: &wgpu::BindGroupLayout,
        surface_format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let source = INFO_TEXT_SHADER.as_source().snapshot();
        let info_text_shader = device.create_shader_module(&wgpu::ShaderModuleDescriptor {
            label: Some("EverythingRenderer::info_text_shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(&*source)),
        });

        let info_text_render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("EverythingRenderer::info_text_render_pipeline_layout"),
                bind_group_layouts: &[info_text_bind_group_layout],
                push_constant_ranges: &[],
            });

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("EverythingRenderer::info_text_render_pipeline"),
            layout: Some(&info_text_render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &info_text_shader,
                entry_point: "vs_main",
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &info_text_shader,
                entry_point: "fs_main",
                targets: &[wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            operation: wgpu::BlendOperation::Add,
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        },
                        // TODO: this probably isn't correct but it doesn't matter until such
                        // time as we get into transparent framebuffers
                        alpha: wgpu::BlendComponent::REPLACE,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                }],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(), // default = off
            multiview: None,
        })
    }

    /// Returns the last [`Viewport`] provided.
    pub fn viewport(&self) -> Viewport {
        self.cameras.viewport()
    }

    /// Sets the expected viewport dimensions to use for the next frame.
    ///
    /// Returns an error if relevant resources could not be allocated.
    pub fn set_viewport(&mut self, viewport: Viewport) -> Result<(), GraphicsResourceError> {
        self.cameras.set_viewport(viewport);

        // wgpu insists on nonzero values
        if viewport.framebuffer_size.x > 0 && viewport.framebuffer_size.y > 0 {
            self.config.width = viewport.framebuffer_size.x;
            self.config.height = viewport.framebuffer_size.y;

            self.info_text_texture.resize(
                &self.device,
                Some("info_text_texture"),
                Vector2::new(
                    viewport.nominal_size.x as u32,
                    viewport.nominal_size.y as u32,
                ),
            );
            self.info_text_bind_group = None;
        }

        Ok(())
    }

    pub async fn render_frame(
        &mut self,
        _cursor_result: &Option<Cursor>,
        queue: &wgpu::Queue,
        output: &wgpu::SurfaceTexture,
        depth_texture_view: &wgpu::TextureView,
    ) -> Result<RenderInfo, GraphicsResourceError> {
        let start_frame_time = Instant::now();

        // This updates camera matrices and graphics options
        self.cameras.update();

        // Recompile shaders if needed.
        // TODO: Recompile block shaders
        if self.info_text_shader_dirty.get_and_clear() {
            self.info_text_render_pipeline = Self::create_info_text_pipeline(
                &self.device,
                &self.info_text_bind_group_layout,
                self.config.format,
            );
        }
        self.block_render_stuff
            .recompile_if_changed(&self.device, self.config.format);

        let output_view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let ws = self.cameras.world_space().snapshot(); // TODO: ugly
        let spaces_to_render = Layers {
            world: ws.as_ref(),
            ui: self.cameras.ui_space(),
        };

        // Make sure we're rendering the right spaces.
        // TODO: we should be able to express this as something like "Layers::for_each_zip()"
        if self.space_renderers.world.as_ref().map(|sr| sr.space()) != spaces_to_render.world {
            self.space_renderers.world = spaces_to_render
                .world
                .cloned()
                .map(|space| {
                    SpaceRenderer::new(
                        space,
                        String::from("world"),
                        &self.device,
                        queue,
                        &self.block_render_stuff,
                    )
                })
                .transpose()?;
        }
        if self.space_renderers.ui.as_ref().map(|sr| sr.space()) != spaces_to_render.ui {
            self.space_renderers.ui = spaces_to_render
                .ui
                .cloned()
                .map(|space| {
                    SpaceRenderer::new(
                        space,
                        String::from("ui"),
                        &self.device,
                        queue,
                        &self.block_render_stuff,
                    )
                })
                .transpose()?;
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("EverythingRenderer::render_frame()"),
            });

        let mut bwp = BeltWritingParts {
            device: &*self.device,
            belt: &mut self.staging_belt,
            encoder: &mut encoder,
        };

        let outputs = Layers {
            world: self
                .space_renderers
                .world
                .as_mut()
                .map(|sr| {
                    sr.prepare_frame(
                        queue,
                        &self.cameras.cameras().world,
                        &self.block_render_stuff,
                        bwp.reborrow(),
                    )
                })
                .transpose()?,
            ui: self
                .space_renderers
                .ui
                .as_mut()
                .map(|sr| {
                    sr.prepare_frame(
                        queue,
                        &self.cameras.cameras().ui,
                        &self.block_render_stuff,
                        bwp,
                    )
                })
                .transpose()?,
        };

        // TODO: Prepare debug lines mesh.

        //let start_staging_time = Instant::now();
        self.staging_belt.finish();
        //let end_staging_time = Instant::now();

        // Done with general preparation (and everything that will write onto the staging belt);
        // move on to draw calls.
        let end_prepare_time = Instant::now();
        let world_render_info = if let Some(so) = outputs.world {
            so.draw(&output_view, depth_texture_view, queue, &mut encoder, true)?
        } else {
            SpaceRenderInfo::default()
        };
        let world_to_ui_time = Instant::now();
        let ui_render_info = if let Some(so) = outputs.ui {
            so.draw(&output_view, depth_texture_view, queue, &mut encoder, false)?
        } else {
            SpaceRenderInfo::default()
        };
        let ui_to_submit_time = Instant::now();

        queue.submit(std::iter::once(encoder.finish()));
        self.staging_belt.recall().await;

        let end_time = Instant::now();
        Ok(RenderInfo {
            frame_time: end_time.duration_since(start_frame_time),
            prepare_time: world_to_ui_time.duration_since(end_prepare_time),
            draw_time: Layers {
                world: world_to_ui_time.duration_since(end_prepare_time),
                ui: ui_to_submit_time.duration_since(world_to_ui_time),
            },
            draw_info: Layers {
                world: world_render_info,
                ui: ui_render_info,
            },
            submit_time: Some(end_time.duration_since(ui_to_submit_time)), // also counting recall()
        })
    }

    pub fn add_info_text(
        &mut self,
        queue: &wgpu::Queue,
        output: &wgpu::SurfaceTexture,
        text: &str,
    ) {
        if text.is_empty() || !self.cameras.cameras().world.options().debug_info_text {
            // TODO: Avoid computing the text, not just drawing it
            return;
        }

        let info_text_texture = &mut self.info_text_texture;
        info_text_texture.draw_target().clear_transparent();
        Text::with_baseline(
            text,
            Point::new(5, 5),
            MonoTextStyle::new(&FONT_7X13_BOLD, Rgb888::new(0, 0, 0)),
            Baseline::Top,
        )
        .draw(info_text_texture.draw_target())
        .unwrap(); // TODO: use .into_ok() when stable
        info_text_texture.upload(queue);

        // TODO: avoid recreating this
        let output_view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("add_info_text() encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("add_info_text() pass"),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });

            render_pass.set_pipeline(&self.info_text_render_pipeline);
            render_pass.set_bind_group(
                0,
                self.info_text_bind_group.get_or_insert_with(|| {
                    self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        layout: &self.info_text_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(
                                    self.info_text_texture.view().unwrap(), // TODO: have a better plan than unwrap
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.info_text_sampler),
                            },
                        ],
                        label: Some("EverythingRenderer::info_text_bind_group"),
                    })
                }),
                &[],
            );
            render_pass.draw(0..3, 0..1);
        }

        queue.submit(std::iter::once(encoder.finish()));
    }
}

fn create_depth_texture(device: &wgpu::Device, everything: &EverythingRenderer) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("SurfaceRenderer::depth_texture"),
        size: wgpu::Extent3d {
            width: everything.config.width,
            height: everything.config.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
    })
}

static INFO_TEXT_SHADER: Lazy<Reloadable> =
    Lazy::new(|| reloadable_str!("src/in_wgpu/shaders/info-text.wgsl"));
