// Copyright 2020-2021 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <https://opensource.org/licenses/MIT>.

//! Top level of the `luminance`-based renderer.

use std::fmt;
use std::time::Duration;

use cgmath::{Matrix4, SquareMatrix};
use embedded_graphics::mono_font::iso_8859_1::FONT_7X13_BOLD;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{Drawable, Point};
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use instant::Instant; // wasm-compatible replacement for std::time::Instant
use luminance::blending::Blending;
use luminance::blending::Equation;
use luminance::blending::Factor;
use luminance::context::GraphicsContext;
use luminance::depth_stencil::Write;
use luminance::framebuffer::Framebuffer;
use luminance::pipeline::PipelineState;
use luminance::render_state::RenderState;
use luminance::tess::Mode;
use luminance::texture::{Dim2, MagFilter, MinFilter};

use crate::apps::{Layers, StandardCameras};
use crate::camera::{Camera, Viewport};
use crate::character::{Character, Cursor};
use crate::content::palette;
use crate::lum::frame_texture::{FullFramePainter, FullFrameTexture};
use crate::lum::shading::{prepare_lines_program, BlockPrograms, LinesProgram, ShaderConstants};
use crate::lum::space::{SpaceRenderInfo, SpaceRenderer};
use crate::lum::types::{AicLumBackend, LinesVertex};
use crate::lum::GraphicsResourceError;
use crate::lum::{make_cursor_tess, wireframe_vertices};
use crate::math::{Aab, Rgba};
use crate::util::{CustomFormat, StatusText};

/// Game world/UI renderer targeting `luminance`.
// TODO: give this and its module a better name
pub struct GLRenderer<C>
where
    C: GraphicsContext,
    C::Backend: AicLumBackend + Sized,
{
    cameras: StandardCameras,

    // Graphics objects
    pub surface: C,
    back_buffer: Framebuffer<C::Backend, Dim2, (), ()>,
    block_programs: Layers<BlockPrograms<C::Backend>>,
    lines_program: LinesProgram<C::Backend>,
    info_text_texture: FullFrameTexture<C::Backend>,

    // Rendering state
    // TODO: use Layers for this
    world_renderer: Option<SpaceRenderer<C::Backend>>,
    ui_renderer: Option<SpaceRenderer<C::Backend>>,
}

impl<C> GLRenderer<C>
where
    C: GraphicsContext,
    C::Backend: AicLumBackend,
{
    /// Constructs `GLRenderer` for the given camera configuration.
    ///
    /// Returns any shader compilation errors or warnings.
    pub fn new(mut surface: C, cameras: StandardCameras) -> Result<Self, GraphicsResourceError> {
        let block_programs = cameras
            .cameras()
            .try_map_ref(|camera| BlockPrograms::compile(&mut surface, camera.options().into()))?;
        // TODO: lines_program is not updated on changed options (and this code should be deduplicated)
        let lines_program =
            prepare_lines_program(&mut surface, cameras.cameras().world.options().into())?;

        let back_buffer = luminance::framebuffer::Framebuffer::back_buffer(
            &mut surface,
            cameras.viewport().framebuffer_size.into(),
        )?;

        let full_frame = FullFramePainter::new(
            &mut surface,
            include_str!("shaders/info-text-fragment.glsl"),
        )?;

        let mut info_text_texture = full_frame.new_texture();
        // TODO: this is duplicated code with set_viewport
        info_text_texture
            .resize(
                &mut surface,
                cameras.viewport(),
                info_text_size_policy,
                (MagFilter::Nearest, MinFilter::Linear),
            )
            .unwrap(); // TODO: texture allocation can fail; handle this gracefully

        Ok(Self {
            surface,
            back_buffer,
            block_programs,
            lines_program,
            info_text_texture,
            world_renderer: None,
            ui_renderer: cameras.ui_space().cloned().map(SpaceRenderer::new),
            cameras,
        })
    }

    /// Returns the last [`Viewport`] provided.
    pub fn viewport(&self) -> Viewport {
        self.cameras.viewport()
    }

    /// Sets the expected viewport dimensions. Use in case of window resizing.
    pub fn set_viewport(&mut self, viewport: Viewport) -> Result<(), GraphicsResourceError> {
        self.cameras.set_viewport(viewport);

        self.back_buffer = luminance::framebuffer::Framebuffer::back_buffer(
            &mut self.surface,
            viewport.framebuffer_size.into(),
        )?;

        // TODO: If this fails, it should be "warning, not error"
        self.info_text_texture.resize(
            &mut self.surface,
            viewport,
            info_text_size_policy,
            (MagFilter::Nearest, MinFilter::Linear),
        )?;

        Ok(())
    }

    /// Sync camera to character state. This is used so that cursor raycasts can be up-to-date
    /// to the same frame of input.
    #[doc(hidden)] // TODO: design better interface that doesn't need to call this
    pub fn update_world_camera(&mut self) {
        self.cameras.update();
    }

    pub fn cameras(&self) -> &StandardCameras {
        &self.cameras
    }

    /// Draw a frame, excluding info text overlay.
    pub fn render_frame(
        &mut self,
        cursor_result: &Option<Cursor>,
    ) -> Result<RenderInfo, GraphicsResourceError> {
        let mut info = RenderInfo::default();
        let start_frame_time = Instant::now();

        // This updates camera matrices and graphics options
        self.cameras.update();
        let graphics_options = self.cameras.graphics_options();

        // Recompile shaders if needed
        // TODO: Layers should have methods to help with this
        let mut update_program = |programs: &mut BlockPrograms<_>, camera: &Camera| {
            let shader_constants: ShaderConstants = camera.options().into();
            if shader_constants != programs.constants {
                match BlockPrograms::compile(&mut self.surface, shader_constants) {
                    Ok(p) => *programs = p,
                    Err(e) => log::error!("Failed to recompile shaders: {}", e),
                }
            }
        };
        update_program(
            &mut self.block_programs.world,
            &self.cameras.cameras().world,
        );
        update_program(&mut self.block_programs.ui, &self.cameras.cameras().ui);

        let block_programs = &mut self.block_programs;

        let surface = &mut self.surface;

        let character: &Character = &*(if let Some(character_ref) = self.cameras.character() {
            character_ref.borrow()
        } else {
            // Nothing to draw; clear screen and exit
            surface
                .new_pipeline_gate()
                .pipeline(&self.back_buffer, &PipelineState::default(), |_, _| Ok(()))
                .assume()
                .into_result()?;
            return Ok(info);
        });

        // Prepare Tess and Texture for space.
        let start_prepare_time = Instant::now();
        if self.world_renderer.as_ref().map(|sr| sr.space()) != Some(&character.space) {
            self.world_renderer = Some(SpaceRenderer::new(character.space.clone()));
        }
        let world_renderer = self.world_renderer.as_mut().unwrap();
        let world_output = world_renderer.prepare_frame(surface, &self.cameras.cameras().world)?;

        let ui_output = if let Some(ui_renderer) = &mut self.ui_renderer {
            Some(ui_renderer.prepare_frame(surface, &self.cameras.cameras().ui)?)
        } else {
            None
        };

        info.prepare_time = Instant::now().duration_since(start_prepare_time);

        let debug_lines_tess = {
            let mut v: Vec<LinesVertex> = Vec::new();

            if graphics_options.debug_collision_boxes {
                // Character collision box
                wireframe_vertices(
                    &mut v,
                    palette::DEBUG_COLLISION_BOX,
                    character.body.collision_box_abs(),
                );
                // What it collided with
                for contact in &character.colliding_cubes {
                    wireframe_vertices(&mut v, palette::DEBUG_COLLISION_CUBES, *contact);
                }
            }

            // Show light update debug info.
            // This is enabled/disabled inside the lighting algorithm, not as a graphics
            // option.
            for cube in character.space.borrow().last_light_updates.iter().copied() {
                wireframe_vertices(
                    &mut v,
                    Rgba::new(1.0, 1.0, 0.0, 1.0),
                    Aab::from_cube(cube).expand(0.005),
                );
            }

            // Lighting trace at cursor
            if graphics_options.debug_light_rays_at_cursor {
                if let Some(cursor) = cursor_result {
                    // TODO: We should be able to draw wireframes in the UI space too, and when we do that will enable supporting this.
                    if cursor.space == character.space {
                        let space = character.space.borrow();
                        let (_, _, _, lighting_info) =
                            space.compute_lighting(cursor.place.adjacent());
                        wireframe_vertices(&mut v, Rgba::new(0.8, 0.8, 1.0, 1.0), lighting_info);
                    }
                }
            }

            // If we have vertices, draw them
            if v.is_empty() {
                None
            } else {
                Some(
                    surface
                        .new_tess()
                        .set_vertices(v)
                        .set_mode(Mode::Line)
                        .build()?,
                )
            }
        };

        // TODO: cache
        let cursor_tess = make_cursor_tess(surface, cursor_result)?;

        let start_draw_world_time = Instant::now();
        surface
            .new_pipeline_gate()
            .pipeline(
                &self.back_buffer,
                // TODO: port skybox cube map code
                &PipelineState::default()
                    .set_clear_color(Some(world_output.data.clear_color().to_srgb_float())),
                |pipeline, mut shading_gate| {
                    let world_output_bound = world_output.bind(&pipeline)?;
                    // Space
                    info.space = world_output_bound.render(
                        &mut shading_gate,
                        &mut block_programs.world,
                        &mut self.lines_program,
                    )?;

                    // Cursor and debug info
                    // Note: This will fall on top of transparent world content due to draw order.
                    shading_gate.shade(
                        &mut self.lines_program,
                        |ref mut program_iface, u, mut render_gate| {
                            u.initialize(program_iface, &world_output_bound, Matrix4::identity());
                            render_gate.render(&RenderState::default(), |mut tess_gate| {
                                // Draw cursor only if it's in the same space.
                                if matches!(cursor_result, Some(c) if c.space == character.space) {
                                    if let Some(tess) = &cursor_tess {
                                        tess_gate.render(tess)?;
                                    }
                                }

                                if let Some(tess) = &debug_lines_tess {
                                    tess_gate.render(tess)?;
                                }
                                Ok(())
                            })?;
                            Ok(())
                        },
                    )
                },
            )
            .assume()
            .into_result()?;

        let start_draw_ui_time = Instant::now();
        surface
            .new_pipeline_gate()
            .pipeline(
                &self.back_buffer,
                // TODO: port skybox cube map code
                &PipelineState::default().set_clear_color(None),
                |ref pipeline, ref mut shading_gate| {
                    if let Some(ui_output) = ui_output {
                        // TODO: Ignoring info
                        ui_output.bind(pipeline)?.render(
                            shading_gate,
                            &mut block_programs.ui,
                            &mut self.lines_program,
                        )?;
                    }
                    Ok(())
                },
            )
            .assume()
            .into_result()?;

        let end_time = Instant::now();
        info.draw_world_time = start_draw_ui_time.duration_since(start_draw_world_time);
        info.draw_ui_time = end_time.duration_since(start_draw_ui_time);
        info.frame_time = end_time.duration_since(start_frame_time);
        Ok(info)
    }

    pub fn add_info_text(&mut self, text: &str) -> Result<(), GraphicsResourceError> {
        if !self.cameras.cameras().world.options().debug_info_text {
            // TODO: Avoid computing the text, not just drawing it
            return Ok(());
        }

        let info_text_texture = &mut self.info_text_texture;
        info_text_texture.data().fill(0);
        Text::with_baseline(
            text,
            Point::new(5, 5),
            MonoTextStyle::new(&FONT_7X13_BOLD, Rgb888::new(0, 0, 0)),
            Baseline::Top,
        )
        .draw(info_text_texture)
        .unwrap(); // TODO: use .into_ok() when stable
        info_text_texture.upload()?;

        self.surface
            .new_pipeline_gate()
            .pipeline(
                &self.back_buffer,
                &PipelineState::default().set_clear_color(None),
                |ref pipeline, ref mut shading_gate| -> Result<(), GraphicsResourceError> {
                    let success = info_text_texture.render(
                        &RenderState::default()
                            .set_depth_write(Write::Off)
                            .set_blending(Some(Blending {
                                equation: Equation::Additive,
                                src: Factor::One,
                                dst: Factor::SrcAlphaComplement,
                            })),
                        pipeline,
                        shading_gate,
                    )?;
                    assert!(success);

                    Ok(())
                },
            )
            .into_result()?;
        Ok(())
    }
}

/// Information about render performance.
#[derive(Clone, Debug, Default)]
pub struct RenderInfo {
    frame_time: Duration,
    prepare_time: Duration,
    draw_world_time: Duration,
    draw_ui_time: Duration,
    space: SpaceRenderInfo,
}

impl CustomFormat<StatusText> for RenderInfo {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>, _: StatusText) -> fmt::Result {
        writeln!(
            fmt,
            "Frame time: {} (prep {}, draw world {}, ui {})",
            self.frame_time.custom_format(StatusText),
            self.prepare_time.custom_format(StatusText),
            self.draw_world_time.custom_format(StatusText),
            self.draw_ui_time.custom_format(StatusText),
        )?;
        write!(fmt, "{}", self.space.custom_format(StatusText))?;
        Ok(())
    }
}

fn info_text_size_policy(mut viewport: Viewport) -> Viewport {
    viewport.framebuffer_size = viewport.nominal_size.map(|c| c.round() as u32);
    viewport
}
