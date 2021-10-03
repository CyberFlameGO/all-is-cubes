// Copyright 2020-2021 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <https://opensource.org/licenses/MIT>.

use cgmath::{EuclideanSpace as _, InnerSpace as _, Point3, Vector3};

use crate::block::{recursive_ray, Evoxel};
use crate::camera::LightingOption;
use crate::math::{Face, FreeCoordinate, GridPoint, Rgb, Rgba};
use crate::raycast::{Ray, Raycaster};
use crate::raytracer::{PixelBuf, SpaceRaytracer, TracingBlock, TracingCubeData};
use crate::space::GridArray;

/// Description of a surface the ray passes through (or from the volumetric perspective,
/// a transition from one material to another).
// TODO: make public?
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Surface<'a, D> {
    pub block_data: &'a D,
    // pub voxel_data: ...?,
    pub diffuse_color: Rgba,
    /// The cube of the [`Space`] which contains the block this surface belongs to.
    cube: GridPoint,
    /// The distance along the ray, in units of the ray's direction vector,
    /// where it intersected the surface.
    pub t_distance: FreeCoordinate,
    /// The point in the [`Space`]'s coordinate system where the ray intersected the surface.
    intersection_point: Point3<FreeCoordinate>,
    pub normal: Face,
}

impl<D> Surface<'_, D> {
    /// Convert the surface and its lighting to a single RGBA value as determined by
    /// the given graphics options, or [`None`] if it is invisible.
    ///
    /// Note this is not true volumetric ray tracing: we're considering each
    /// voxel surface to be discrete.
    #[inline]
    pub(crate) fn to_lit_color<P>(&self, rt: &SpaceRaytracer<P>) -> Option<Rgba>
    where
        P: PixelBuf<BlockData = D>,
        D: 'static,
    {
        let diffuse_color = rt.options.transparency.limit_alpha(self.diffuse_color);
        if diffuse_color.fully_transparent() {
            return None;
        }
        let adjusted_rgb = diffuse_color.to_rgb()
            * self.compute_illumination(rt)
            * fixed_directional_lighting(self.normal);
        Some(adjusted_rgb.with_alpha(diffuse_color.alpha()))
    }

    pub fn compute_illumination<P>(&self, rt: &SpaceRaytracer<P>) -> Rgb
    where
        P: PixelBuf<BlockData = D>,
        D: 'static,
    {
        match rt.options.lighting_display {
            LightingOption::None => Rgb::ONE,
            LightingOption::Flat => rt.get_lighting(self.cube + self.normal.normal_vector()),
            LightingOption::Smooth => {
                rt.get_interpolated_light(self.intersection_point, self.normal)
            }
        }
    }
}

/// Simple directional lighting used to give corners extra definition.
/// Note that this algorithm is also implemented in the fragment shader for GPU rendering.
fn fixed_directional_lighting(face: Face) -> f32 {
    let normal = face.normal_vector();
    const LIGHT_1_DIRECTION: Vector3<f32> = Vector3::new(0.4, -0.1, 0.0);
    const LIGHT_2_DIRECTION: Vector3<f32> = Vector3::new(-0.4, 0.35, 0.25);
    (1.0 - 1.0 / 16.0)
        + 0.25 * (LIGHT_1_DIRECTION.dot(normal).max(0.0) + LIGHT_2_DIRECTION.dot(normal).max(0.0))
}

/// Builds on [`Surface`] to report the depth (length of ray through volume)
/// of a transparent surface.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Span<'a, D> {
    pub surface: Surface<'a, D>,
    /// Distance along the ray at which the ray exits this volume.
    ///
    /// The `surface.t_distance` is the corresponding entry point.
    pub exit_t_distance: FreeCoordinate,
}

/// Output of [`SurfaceIter`], describing a single step of the raytracing process.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum TraceStep<'a, D> {
    /// A block or voxel surface as described was entered.
    ///
    /// Note that when the surface is invisible (alpha is zero or treated as zero),
    /// [`TraceStep::Invisible`] is returned.
    EnterSurface(Surface<'a, D>),
    /// A completely invisible surface was found.
    /// This is reported so that it may be counted in a decision to stop tracing.
    /// It is separate from [`TraceStep::EnterSurface`] to avoid computing lighting.
    Invisible { t_distance: FreeCoordinate },
    /// A recursive block was entered.
    ///
    /// This is reported so as to keep the iteration process O(1).
    /// In particular, otherwise an unbounded number of steps could be taken if the ray
    /// passes through a large number of recursive blocks with small bounds (such that no
    /// actual voxels exist to be visible or invisible).
    EnterBlock { t_distance: FreeCoordinate },
    // TODO: Add 'enter space' and 'exit space' possibly.
}

/// An [`Iterator`] which reports each visible surface a [`Raycaster`] ray passes through.
// TODO: make public?
#[derive(Clone, Debug)]
pub(crate) struct SurfaceIter<'a, D: 'static> {
    ray: Ray,
    block_raycaster: Raycaster,
    current_block: Option<VoxelSurfaceIter<'a, D>>,
    blocks: &'a [TracingBlock<D>],
    array: &'a GridArray<TracingCubeData>,
}

impl<'a, D: 'static> SurfaceIter<'a, D> {
    #[inline]
    pub(crate) fn new<P>(rt: &'a SpaceRaytracer<P>, ray: Ray) -> Self
    where
        P: PixelBuf<BlockData = D>,
    {
        Self {
            ray,
            block_raycaster: ray.cast().within_grid(rt.cubes.grid()),
            current_block: None,
            blocks: &rt.blocks,
            array: &rt.cubes,
        }
    }
}

impl<'a, D: 'static> Iterator for SurfaceIter<'a, D> {
    type Item = TraceStep<'a, D>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(b) = &mut self.current_block {
            if let Some(surface) = b.next() {
                return Some(surface);
            }
        }
        // If we get here, self.current_block is either exhausted or already None
        self.current_block = None;

        let rc_step = self.block_raycaster.next()?;

        let cube_data: &TracingCubeData = &self.array[rc_step.cube_ahead()];
        if cube_data.always_invisible {
            // Early return that avoids indirecting through self.blocks
            return Some(TraceStep::Invisible {
                t_distance: rc_step.t_distance(),
            });
        }

        Some(match &self.blocks[cube_data.block_index as usize] {
            TracingBlock::Atom(block_data, color) => {
                if color.fully_transparent() {
                    // The caller could generically skip transparent, but if we do it then
                    // we can skip some math too.
                    TraceStep::Invisible {
                        t_distance: rc_step.t_distance(),
                    }
                } else {
                    TraceStep::EnterSurface(Surface {
                        block_data,
                        diffuse_color: *color,
                        cube: rc_step.cube_ahead(),
                        t_distance: rc_step.t_distance(),
                        intersection_point: rc_step.intersection_point(self.ray),
                        normal: rc_step.face(),
                    })
                }
            }
            TracingBlock::Recur(block_data, resolution, array) => {
                let block_cube = rc_step.cube_ahead();
                let resolution = *resolution;
                let sub_ray = recursive_ray(self.ray, block_cube, resolution);
                let antiscale = FreeCoordinate::from(resolution).recip();

                self.current_block = Some(VoxelSurfaceIter {
                    voxel_ray: sub_ray,
                    voxel_raycaster: sub_ray.cast().within_grid(array.grid()),
                    block_data,
                    antiscale,
                    array,
                    block_cube,
                });

                TraceStep::EnterBlock {
                    t_distance: rc_step.t_distance(),
                }
            }
        })
    }

    // TODO: implement fold if it helps
}

/// Iterates over a [`Block`]'s voxels. Internal helper for [`SurfaceIter`].
#[derive(Clone, Debug)]
struct VoxelSurfaceIter<'a, D: 'static> {
    voxel_ray: Ray,
    voxel_raycaster: Raycaster,
    block_data: &'a D,
    /// Reciprocal of resolution, for scaling back to outer world
    antiscale: FreeCoordinate,
    array: &'a GridArray<Evoxel>,

    /// Cube these voxels are located in, for lighting lookups.
    block_cube: GridPoint,
}
impl<'a, D> VoxelSurfaceIter<'a, D> {
    /// This is not an  implementation of `Iterator` because it doesn't need to be — it's
    /// purely internal to [`SurfaceIter`].
    fn next(&mut self) -> Option<TraceStep<'a, D>> {
        // Fetch data and return None if out of range.
        let rc_step = self.voxel_raycaster.next()?;
        let voxel = self.array.get(rc_step.cube_ahead())?;

        if voxel.color.fully_transparent() {
            return Some(TraceStep::Invisible {
                t_distance: rc_step.t_distance() * self.antiscale,
            });
        }

        Some(TraceStep::EnterSurface(Surface {
            block_data: self.block_data,
            diffuse_color: voxel.color,
            cube: self.block_cube,
            // Note: The proper scaling here depends on the direction vector scale, that
            // recursive_ray() _doesn't_ change.
            t_distance: rc_step.t_distance() * self.antiscale,
            intersection_point: rc_step.intersection_point(self.voxel_ray) * self.antiscale
                + self.block_cube.map(FreeCoordinate::from).to_vec(),
            normal: rc_step.face(),
        }))
    }
}

/// Builds on [`SurfaceIter`] to report spans of transparency along the ray.
pub(crate) struct DepthIter<'a, D: 'static> {
    surface_iter: SurfaceIter<'a, D>,
    /// Present if the last `EnterSurface` we discovered was transparent, or if
    /// we have another surface to report.
    last_surface: Option<Surface<'a, D>>,
}

impl<'a, D> DepthIter<'a, D> {
    #[inline]
    pub(crate) fn new(surface_iter: SurfaceIter<'a, D>) -> Self {
        Self {
            surface_iter,
            last_surface: None,
        }
    }
}

impl<'a, D> Iterator for DepthIter<'a, D> {
    type Item = DepthStep<'a, D>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // TODO: .next()? isn't quite right; we need to flush the last input, potentially? Or, no, that should never happen, but we should assert that.
        Some(match self.surface_iter.next()? {
            TraceStep::EnterSurface(this_surface) => {
                let exit_t_distance = this_surface.t_distance;
                match std::mem::replace(&mut self.last_surface, Some(this_surface)) {
                    Some(last_surface) => DepthStep::Span(Span {
                        surface: last_surface,
                        exit_t_distance,
                    }),
                    None => DepthStep::Invisible,
                }
            }
            TraceStep::Invisible { t_distance } | TraceStep::EnterBlock { t_distance } => {
                match self.last_surface.take() {
                    Some(last_surface) => DepthStep::Span(Span {
                        surface: last_surface,
                        exit_t_distance: t_distance,
                    }),
                    None => DepthStep::Invisible,
                }
            }
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum DepthStep<'a, D> {
    Invisible,
    Span(Span<'a, D>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;
    use crate::camera::GraphicsOptions;
    use crate::content::{make_slab, palette};
    use crate::raytracer::ColorBuf;
    use crate::space::{Grid, Space};
    use crate::universe::Universe;
    use pretty_assertions::assert_eq;
    use TraceStep::{EnterBlock, EnterSurface, Invisible};

    #[test]
    fn surface_iter_smoke_test() {
        let universe = &mut Universe::new();

        let mut space = Space::builder(Grid::new([0, 0, 0], [1, 3, 1])).build_empty();

        let solid_test_color = rgba_const!(1., 0., 0., 1.);
        space.set([0, 1, 0], Block::from(solid_test_color)).unwrap();
        space.set([0, 2, 0], make_slab(universe, 2, 4)).unwrap();

        let rt = SpaceRaytracer::<ColorBuf>::new(&space, GraphicsOptions::default());

        assert_eq!(
            SurfaceIter::new(&rt, Ray::new([0.5, -0.5, 0.5], [0., 1., 0.]))
                .collect::<Vec<TraceStep<'_, ()>>>(),
            vec![
                Invisible { t_distance: 0.5 }, // The within-origin-block step
                EnterSurface(Surface {
                    block_data: &(),
                    diffuse_color: solid_test_color,
                    cube: GridPoint::new(0, 1, 0),
                    t_distance: 1.5, // half-block starting point + 1 empty block
                    intersection_point: Point3::new(0.5, 1.0, 0.5),
                    normal: Face::NY
                }),
                EnterBlock { t_distance: 2.5 },
                EnterSurface(Surface {
                    block_data: &(),
                    diffuse_color: palette::PLANK.with_alpha_one(),
                    cube: GridPoint::new(0, 2, 0),
                    t_distance: 2.5,
                    intersection_point: Point3::new(0.5, 2.0, 0.5),
                    normal: Face::NY
                }),
                // Second layer of slab.
                // TODO: Make this test not dependent on make_slab's colors,
                // perhaps by making the color an input to make_slab.
                EnterSurface(Surface {
                    block_data: &(),
                    diffuse_color: (palette::PLANK * 1.06).with_alpha_one(),
                    cube: GridPoint::new(0, 2, 0),
                    t_distance: 2.75, // previous surface + 1/4 block of depth
                    intersection_point: Point3::new(0.5, 2.25, 0.5),
                    normal: Face::NY
                }),
                // Two top layers of slab.
                Invisible { t_distance: 3.0 },
                Invisible { t_distance: 3.25 },
                // TODO: need to report exiting the block and/or space, for the sake of volumetric computation
            ]
        );
    }
}
