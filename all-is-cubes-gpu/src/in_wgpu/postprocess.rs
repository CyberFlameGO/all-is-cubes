//! By “postprocessing” we mean:
//! * compositing previously rendered content into a final image
//! * screen-space effects such as bloom
//! * tone mapping

use std::sync::Arc;

use once_cell::sync::Lazy;

use all_is_cubes::camera::{GraphicsOptions, ToneMappingOperator};

use crate::in_wgpu::glue::create_wgsl_module_from_reloadable;
use crate::reloadable::{reloadable_str, Reloadable};

pub(crate) static POSTPROCESS_SHADER: Lazy<Reloadable> =
    Lazy::new(|| reloadable_str!("src/in_wgpu/shaders/postprocess.wgsl"));

pub(crate) fn create_postprocess_bind_group_layout(
    device: &Arc<wgpu::Device>,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        entries: &[
            // Binding for info_text_texture
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
            // Binding for info_text_sampler
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // Binding for linear_scene_texture
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
            // Binding for camera
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            // Binding for bloom_texture
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
        ],
        label: Some("postprocess_bind_group_layout"),
    })
}

/// Read postprocessing shader and create the postprocessing render pipeline.
pub(crate) fn create_postprocess_pipeline(
    device: &wgpu::Device,
    postprocess_bind_group_layout: &wgpu::BindGroupLayout,
    surface_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let postprocess_shader = create_wgsl_module_from_reloadable(
        device,
        "EverythingRenderer::postprocess_shader",
        &POSTPROCESS_SHADER,
    );

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("EverythingRenderer::postprocess_pipeline_layout"),
        bind_group_layouts: &[postprocess_bind_group_layout],
        push_constant_ranges: &[],
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("EverythingRenderer::postprocess_render_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &postprocess_shader,
            entry_point: "postprocess_vertex",
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &postprocess_shader,
            entry_point: "postprocess_fragment",
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
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
        // default = off. No need for multisampling since we are not drawing triangles here.
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    })
}

/// Information corresponding to [`Camera`] (or, for the moment, just [`GraphicsOptions`])
/// but in a form suitable for passing in a uniform buffer to the `postprocess.wgsl`
/// shader.
#[repr(C, align(16))] // align triggers bytemuck error if the size doesn't turn out to be a multiple
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct PostprocessUniforms {
    tone_mapping_id: i32,

    /// 0 or 1 boolean indicating whether or not the `linear_scene_texture` was actually
    /// written this frame. If zero, the postprocessing shader should display a “no data”
    /// indication instead of reading the scene texture.
    texture_is_valid: i32,

    bloom_intensity: f32,

    /// pad out to multiple of vec4<something32>
    _padding: f32,
}

impl PostprocessUniforms {
    pub(crate) fn new(options: &GraphicsOptions, texture_is_valid: bool) -> Self {
        Self {
            tone_mapping_id: match options.tone_mapping {
                ToneMappingOperator::Clamp => 0,
                ToneMappingOperator::Reinhard => 1,
                ref tmo => panic!("Missing implementation for tone mapping operator {tmo:?}"),
            },

            texture_is_valid: i32::from(texture_is_valid),

            bloom_intensity: options.bloom_intensity.into_inner(),

            _padding: Default::default(),
        }
    }
}

pub(crate) fn postprocess(
    // TODO: instead of accepting `EverythingRenderer`, pass smaller (but not too numerous) things
    ev: &mut super::EverythingRenderer,
    queue: &wgpu::Queue,
    output: &wgpu::Texture,
) {
    let mut encoder = ev
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("add_info_text_and_postprocess() encoder"),
        });

    // Render pass
    {
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("add_info_text_and_postprocess() pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &output_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: true,
                },
            })],
            depth_stencil_attachment: None,
        });

        render_pass.set_pipeline(&ev.postprocess_render_pipeline);
        render_pass.set_bind_group(
            0,
            ev.postprocess_bind_group.get_or_insert_with(|| {
                ev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    layout: &ev.postprocess_bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(
                                ev.info_text_texture.view().unwrap(), // TODO: have a better plan than unwrap
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&ev.info_text_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(
                                ev.fb.scene_for_postprocessing_input(),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: ev.postprocess_camera_buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(
                                ev.fb.bloom_data_texture(),
                            ),
                        },
                    ],
                    label: Some("EverythingRenderer::postprocess_bind_group"),
                })
            }),
            &[],
        );
        render_pass.draw(0..3, 0..1);
    }

    queue.submit(std::iter::once(encoder.finish()));
}
