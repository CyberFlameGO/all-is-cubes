// Copyright 2020-2021 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <https://opensource.org/licenses/MIT>.

//! Definition of blocks, which are game objects which live in the grid of a
//! [`Space`]. See [`Block`] for details.

use std::borrow::Cow;
use std::fmt;

use cgmath::{EuclideanSpace as _, Point3, Vector4, Zero as _};

use crate::listen::Listener;
use crate::math::{
    FreeCoordinate, GridCoordinate, GridPoint, GridRotation, OpacityCategory, Rgb, Rgba,
};
use crate::raycast::{Ray, Raycaster};
use crate::space::{Grid, GridArray, SetCubeError, Space, SpaceChange};
use crate::universe::{RefError, URef};
use crate::util::{ConciseDebug, CustomFormat};

mod attributes;
pub use attributes::*;

mod block_def;
pub use block_def::*;

pub mod builder;
#[doc(inline)]
pub use builder::BlockBuilder;

#[cfg(test)]
mod tests;

/// Type for the edge length of recursive blocks in terms of their component voxels.
/// This resolution cubed is the number of voxels making up a block.
///
/// This type was chosen as `u8` so as to make it nonnegative and easy to losslessly
/// convert into larger, possibly signed, sizes. It's plenty of range since a resolution
/// of 255 would mean 16 million voxels — more than we want to work with.
pub type Resolution = u8;

/// A `Block` is something that can exist in the grid of a [`Space`]; it occupies one unit
/// cube of space and has a specified appearance and behavior.
///
/// In general, when a block appears multiple times from an in-game perspective, that may
/// or may not be the the same copy; `Block`s are "by value". However, some blocks are
/// defined by reference to shared mutable data, in which case changes to that data should
/// take effect everywhere a `Block` having that same reference occurs.
///
/// To obtain the concrete appearance and behavior of a block, use [`Block::evaluate`] to
/// obtain an [`EvaluatedBlock`] value, preferably with caching.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
//#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum Block {
    /// A block whose definition is stored in a [`Universe`](crate::universe::Universe).
    Indirect(URef<BlockDef>),

    /// A block that is a single-colored unit cube. (It may still be be transparent or
    /// non-solid to physics.)
    Atom(BlockAttributes, Rgba),

    /// A block that is composed of smaller blocks, defined by the referenced `Space`.
    Recur {
        attributes: BlockAttributes,
        /// Which portion of the space will be used, specified by the most negative
        /// corner.
        offset: GridPoint,
        /// The side length of the cubical volume of sub-blocks (voxels) used for this
        /// block.
        resolution: u8,
        space: URef<Space>,
    },

    /// Identical to another block, but with rotated coordinates.
    ///
    /// Specifically, the given rotation specifies how the contained block's coordinate
    /// system is rotated into this block's.
    // TODO: Hmm, it'd be nice if this common case wasn't another allocation — should we
    // have an outer struct with a rotation field instead??
    Rotated(GridRotation, Box<Block>),
}

impl Block {
    /// Returns a new [`BlockBuilder`] which may be used to construct a [`Block`] value
    /// from various inputs with convenient syntax.
    pub const fn builder() -> BlockBuilder<builder::NeedsColorOrVoxels> {
        BlockBuilder::<builder::NeedsColorOrVoxels>::new()
    }

    /// Rotates this block by the specified rotation.
    ///
    /// Compared to direct use of the [`Block::Rotated`] variant, this will:
    /// * Avoid constructing chains of `Block::Rotated(Block::Rotated(...))`.
    /// * Not rotate blocks that should never appear rotated (including atom blocks).
    ///
    /// ```
    /// use all_is_cubes::block::{AIR, Block};
    /// use all_is_cubes::content::make_some_voxel_blocks;
    /// use all_is_cubes::math::{Face::*, GridRotation};
    /// use all_is_cubes::universe::Universe;
    ///
    /// let mut universe = Universe::new();
    /// let [block] = make_some_voxel_blocks(&mut universe);
    /// let clockwise = GridRotation::CLOCKWISE;
    ///
    /// // Basic rotation
    /// let rotated = block.clone().rotate(clockwise);
    /// assert_eq!(rotated, Block::Rotated(clockwise, Box::new(block.clone())));
    ///
    /// // Multiple rotations are combined
    /// let double = rotated.clone().rotate(clockwise);
    /// assert_eq!(double, Block::Rotated(clockwise * clockwise, Box::new(block.clone())));
    /// // AIR is never rotated
    /// assert_eq!(AIR, AIR.rotate(clockwise));
    /// ```
    pub fn rotate(self, rotation: GridRotation) -> Self {
        match self {
            // TODO: Just checking for Block::Atom doesn't help when the atom
            // is hidden behind Block::Indirect. In general, we need to evaluate()
            // (which suggests that this perhaps should be at least available
            // as a function that takes Block + EvaluatedBlock).
            Block::Atom(..) => self,
            Block::Rotated(existing_rotation, boxed_block) => {
                // TODO: If the combined rotation is the identity, simplify
                Block::Rotated(rotation * existing_rotation, boxed_block)
            }
            _ => Block::Rotated(rotation, Box::new(self)),
        }
    }

    /// Standardizes any characteristics of this block which may be presumed to be
    /// specific to its usage in its current location, so that it can be used elsewhere
    /// or compared with others. Currently, this means removing rotation, but in the
    /// there may be additional or customizable changes (hence the abstract name).
    ///
    /// ```
    /// use all_is_cubes::block::Block;
    /// use all_is_cubes::content::make_some_voxel_blocks;
    /// use all_is_cubes::math::{Face::*, GridRotation};
    /// use all_is_cubes::universe::Universe;
    ///
    /// let mut universe = Universe::new();
    /// let [block] = make_some_voxel_blocks(&mut universe);
    /// let clockwise = GridRotation::from_basis([PZ, PY, NX]);
    /// let rotated = block.clone().rotate(clockwise);
    /// assert_ne!(&block, &rotated);
    /// assert_eq!(block, rotated.clone().unspecialize());
    /// assert_eq!(block, rotated.clone().unspecialize().unspecialize());
    /// ```
    pub fn unspecialize(self) -> Self {
        match self {
            Block::Rotated(_rotation, boxed_block) => *boxed_block,
            other => other,
        }
    }

    /// Converts this `Block` into a “flattened” and snapshotted form which contains all
    /// information needed for rendering and physics, and does not require [`URef`] access
    /// to other objects.
    pub fn evaluate(&self) -> Result<EvaluatedBlock, EvalBlockError> {
        self.evaluate_impl(0)
    }

    #[inline]
    fn evaluate_impl(&self, depth: u8) -> Result<EvaluatedBlock, EvalBlockError> {
        match self {
            Block::Indirect(def_ref) => def_ref.try_borrow()?.evaluate_impl(next_depth(depth)?),

            &Block::Atom(ref attributes, color) => Ok(EvaluatedBlock {
                attributes: attributes.clone(),
                color,
                voxels: None,
                resolution: 1,
                opaque: color.fully_opaque(),
                visible: !color.fully_transparent(),
                voxel_opacity_mask: if color.fully_transparent() {
                    None
                } else {
                    Some(
                        GridArray::from_elements(Grid::for_block(1), [color.opacity_category()])
                            .unwrap(),
                    )
                },
            }),

            &Block::Recur {
                ref attributes,
                offset,
                resolution,
                space: ref space_ref,
            } => {
                let block_space = space_ref.try_borrow()?;

                // Don't produce a resolution of 0, as that might cause division-by-zero messes later.
                // TODO: Actually, should this be an EvalBlockError instead?
                if resolution == 0 {
                    return Ok(EvaluatedBlock {
                        attributes: attributes.clone(),
                        color: Rgba::TRANSPARENT,
                        voxels: None,
                        resolution: 1,
                        opaque: false,
                        visible: false,
                        voxel_opacity_mask: None,
                    });
                }

                let resolution_g: GridCoordinate = resolution.into();
                let full_resolution_grid =
                    Grid::new(offset, [resolution_g, resolution_g, resolution_g]);
                let occupied_grid = full_resolution_grid
                    .intersection(block_space.grid())
                    .unwrap_or_else(|| Grid::new(offset, [1, 1, 1]) /* arbitrary value */);

                let voxels = block_space
                    .extract(
                        occupied_grid,
                        #[inline(always)]
                        |_index, sub_block_data, _lighting| {
                            Evoxel::from_block(sub_block_data.evaluated())
                        },
                    )
                    .translate(-offset.to_vec());

                Ok(EvaluatedBlock::from_voxels(
                    attributes.clone(),
                    resolution,
                    voxels,
                ))
            }

            // TODO: this has no unit tests
            Block::Rotated(rotation, block) => {
                let base = block.evaluate()?;
                if base.voxels.is_none() && base.voxel_opacity_mask.is_none() {
                    // Skip computation of transforms
                    return Ok(base);
                }

                // TODO: Add a shuffle-in-place rotation operation to GridArray and try implementing this using that, which should have less arithmetic involved than these matrix ops
                let resolution = base.resolution;
                let inner_to_outer = rotation.to_positive_octant_matrix(resolution.into());
                let outer_to_inner = rotation
                    .inverse()
                    .to_positive_octant_matrix(resolution.into());

                Ok(EvaluatedBlock {
                    voxels: base.voxels.map(|voxels| {
                        GridArray::from_fn(
                            voxels.grid().transform(inner_to_outer).unwrap(),
                            |cube| voxels[outer_to_inner.transform_cube(cube)],
                        )
                    }),
                    voxel_opacity_mask: base.voxel_opacity_mask.map(|mask| {
                        GridArray::from_fn(mask.grid().transform(inner_to_outer).unwrap(), |cube| {
                            mask[outer_to_inner.transform_cube(cube)]
                        })
                    }),

                    // Unaffected
                    attributes: base.attributes,
                    color: base.color,
                    resolution,
                    opaque: base.opaque,
                    visible: base.visible,
                })
            }
        }
        // TODO: need to track which things we need change notifications on
    }

    /// Registers a listener for mutations of any data sources which may affect this
    /// block's [`Block::evaluate`] result.
    ///
    /// Note that this does not listen for mutations of the `Block` value itself —
    /// which would be impossible since it is an enum and all its fields
    /// are public. In contrast, [`BlockDef`] does perform such tracking.
    ///
    /// This may fail under the same conditions as [`Block::evaluate`]; it returns the
    /// same error type so that callers which both evaluate and listen don't need to
    /// handle this separately.
    pub fn listen(
        &self,
        listener: impl Listener<BlockChange> + Send + Sync + 'static,
    ) -> Result<(), EvalBlockError> {
        self.listen_impl(listener, 0)
    }

    fn listen_impl(
        &self,
        listener: impl Listener<BlockChange> + Send + Sync + 'static,
        _depth: u8,
    ) -> Result<(), EvalBlockError> {
        match self {
            Block::Indirect(def_ref) => {
                // Note: This does not pass the recursion depth because BlockDef provides
                // its own internal listening and thus this does not recurse.
                def_ref.try_borrow()?.listen(listener)?;
            }
            Block::Atom(_, _) => {
                // Atoms don't refer to anything external and thus cannot change other
                // than being directly overwritten, which is out of the scope of this
                // operation.
            }
            Block::Recur {
                resolution,
                offset,
                space: space_ref,
                ..
            } => {
                let relevant_cubes = Grid::for_block(*resolution).translate(offset.to_vec());
                space_ref.try_borrow()?.listen(listener.filter(move |msg| {
                    match msg {
                        SpaceChange::Block(cube) if relevant_cubes.contains_cube(cube) => {
                            Some(BlockChange::new())
                        }
                        SpaceChange::Block(_) => None,
                        SpaceChange::EveryBlock => Some(BlockChange::new()),

                        // TODO: It would be nice if the space gave more precise updates such that we could conclude
                        // e.g. "this is a new/removed block in an unaffected area" without needing to store any data.
                        SpaceChange::BlockValue(_) => Some(BlockChange::new()),
                        SpaceChange::Lighting(_) => None,
                        SpaceChange::Number(_) => None,
                    }
                }));
            }
            Block::Rotated(_, base) => {
                base.listen(listener)?;
            }
        }
        Ok(())
    }

    /// Returns the single [Rgba] color of this block, or panics if it does not have a
    /// single color. For use in tests only.
    #[cfg(test)]
    pub fn color(&self) -> Rgba {
        match self {
            Block::Atom(_, c) => *c,
            _ => panic!("Block::color not defined for non-atom blocks"),
        }
    }
}

/// Recursion limiter helper for evaluate.
fn next_depth(depth: u8) -> Result<u8, EvalBlockError> {
    if depth > 32 {
        Err(EvalBlockError::StackOverflow)
    } else {
        Ok(depth + 1)
    }
}

// Implementing conversions to `Cow` allow various functions to accept either an owned
// or borrowed `Block`. The motivation for this is to avoid unnecessary cloning
// (in case an individual block has large data).

impl From<Block> for Cow<'_, Block> {
    fn from(block: Block) -> Self {
        Cow::Owned(block)
    }
}
impl<'a> From<&'a Block> for Cow<'a, Block> {
    fn from(block: &'a Block) -> Self {
        Cow::Borrowed(block)
    }
}
/// Convert a color to a block with default attributes.
impl From<Rgb> for Block {
    fn from(color: Rgb) -> Self {
        Block::from(color.with_alpha_one())
    }
}
/// Convert a color to a block with default attributes.
impl From<Rgba> for Block {
    fn from(color: Rgba) -> Self {
        Block::Atom(BlockAttributes::default(), color)
    }
}
/// Convert a color to a block with default attributes.
impl From<Rgb> for Cow<'_, Block> {
    fn from(color: Rgb) -> Self {
        Cow::Owned(Block::from(color))
    }
}
/// Convert a color to a block with default attributes.
impl From<Rgba> for Cow<'_, Block> {
    fn from(color: Rgba) -> Self {
        Cow::Owned(Block::from(color))
    }
}

/// Generic 'empty'/'null' block. It is used by [`Space`] to respond to out-of-bounds requests.
///
/// See also [`AIR_EVALUATED`].
pub const AIR: Block = Block::Atom(AIR_ATTRIBUTES, Rgba::TRANSPARENT);

/// The result of <code>[AIR].[evaluate()](Block::evaluate)</code>, as a constant.
/// This may be used when an [`EvaluatedBlock`] value is needed but there is no block
/// value.
///
/// ```
/// use all_is_cubes::block::{AIR, AIR_EVALUATED};
///
/// assert_eq!(Ok(AIR_EVALUATED), AIR.evaluate());
/// ```
pub const AIR_EVALUATED: EvaluatedBlock = EvaluatedBlock {
    attributes: AIR_ATTRIBUTES,
    color: Rgba::TRANSPARENT,
    voxels: None,
    resolution: 1,
    opaque: false,
    visible: false,
    voxel_opacity_mask: None,
};

const AIR_ATTRIBUTES: BlockAttributes = BlockAttributes {
    display_name: Cow::Borrowed("<air>"),
    selectable: false,
    collision: BlockCollision::None,
    rotation_rule: RotationPlacementRule::Never,
    light_emission: Rgb::ZERO,
    animation_hint: AnimationHint::UNCHANGING,
};

/// A “flattened” and snapshotted form of [`Block`] which contains all information needed
/// for rendering and physics, and does not require dereferencing [`URef`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct EvaluatedBlock {
    /// The block's attributes.
    pub attributes: BlockAttributes,
    /// The block's color; if made of multiple voxels, then an average or representative
    /// color.
    pub color: Rgba,
    /// The voxels making up the block, if any; if [`None`], then [`Self::color`]
    /// should be used as a uniform color value.
    ///
    /// This array may be smaller than the dimensions implied by [`Self::resolution`];
    /// in which case the out-of-bounds space should be treated as [`Evoxel::AIR`].
    /// The logical bounds are always the cube computed by [`Grid::for_block`].
    pub voxels: Option<GridArray<Evoxel>>,
    /// If [`Self::voxels`] is present, then this is the voxel resolution (number of
    /// voxels along an edge) of the block.
    ///
    /// If [`Self::voxels`] is [`None`], then this value is irrelevant and should be set
    /// to 1.
    pub resolution: Resolution,
    /// Whether the block is known to be completely opaque to light on all six faces.
    ///
    /// Currently, this is defined to be that each of the surfaces of the block are
    /// fully opaque, but in the future it might be refined to permit concave surfaces.
    // TODO: generalize opaque to multiple faces and partial opacity, for better light transport
    pub opaque: bool,
    /// Whether the block has any voxels/color at all that make it visible; that is, this
    /// is false if the block is completely transparent.
    pub visible: bool,
    /// The opacity of all voxels. This is redundant with the data  [`Self::voxels`],
    /// and is provided as a pre-computed convenience that can be cheaply compared with
    /// other values of the same type.
    ///
    /// May be [`None`] if the block is fully invisible. (TODO: This is a kludge to avoid
    /// obligating [`AIR_EVALUATED`] to allocate at compile time, which is impossible.
    /// It doesn't harm normal operation because the point of having this is to compare
    /// block shapes, which is trivial if the block is invisible.)
    pub(crate) voxel_opacity_mask: Option<GridArray<OpacityCategory>>,
}

// TODO: Wait, this isn't really what ConciseDebug is for... shouldn't this be a regular impl Debug?
impl CustomFormat<ConciseDebug> for EvaluatedBlock {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>, _: ConciseDebug) -> fmt::Result {
        fmt.debug_struct("EvaluatedBlock")
            .field("attributes", &self.attributes)
            .field("color", &self.color)
            .field("opaque", &self.opaque)
            .field("visible", &self.visible)
            .field("resolution", &self.resolution)
            .field("voxels", &"...")
            .field("voxel_opacity_mask", &"...")
            .finish()
    }
}

impl EvaluatedBlock {
    /// Computes the derived values of a voxel block.
    fn from_voxels(
        attributes: BlockAttributes,
        resolution: Resolution,
        voxels: GridArray<Evoxel>,
    ) -> EvaluatedBlock {
        // Compute color sum from voxels
        // TODO: Give GridArray an iter() or something
        // TODO: The color sum actually needs to be weighted by alpha. (Too bad we're not using premultiplied alpha.)
        // TODO: Should not be counting interior voxels for the color, only visible surfaces.
        let mut color_sum: Vector4<f32> = Vector4::zero();
        for position in voxels.grid().interior_iter() {
            color_sum += voxels[position].color.into();
        }

        let full_block_grid = Grid::for_block(resolution);
        EvaluatedBlock {
            attributes,
            // The single color is the mean of the actual block colors.
            color: Rgba::try_from(
                (color_sum.truncate() / (voxels.grid().volume() as f32))
                    .extend(color_sum.w / (full_block_grid.volume() as f32)),
            )
            .expect("Recursive block color computation produced NaN"),
            resolution,
            // TODO wrong test: we want to see if the _faces_ are all opaque but allow hollows
            opaque: voxels.grid() == full_block_grid
                && voxels.grid().interior_iter().all(
                    #[inline(always)]
                    |p| voxels[p].color.fully_opaque(),
                ),
            visible: voxels.grid().interior_iter().any(
                #[inline(always)]
                |p| !voxels[p].color.fully_transparent(),
            ),
            voxel_opacity_mask: Some(GridArray::from_fn(voxels.grid(), |p| {
                voxels[p].color.opacity_category()
            })),

            voxels: Some(voxels),
        }
    }

    /// Returns whether [`Self::visible`] is true (the block has some visible color/voxels)
    /// or [`BlockAttributes::animation_hint`] indicates that the block might _become_
    /// visible (by change of evaluation result rather than by being replaced).
    #[inline]
    pub(crate) fn visible_or_animated(&self) -> bool {
        self.visible || self.attributes.animation_hint.might_become_visible()
    }
}

/// Errors resulting from [`Block::evaluate`].
#[derive(Clone, Debug, Eq, Hash, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalBlockError {
    #[error("block definition contains too much recursion")]
    StackOverflow,
    /// This may be temporary or permanent.
    #[error("block data inaccessible: {0}")]
    DataRefIs(#[from] RefError),
}

/// Properties of an individual voxel within [`EvaluatedBlock`].
///
/// This is essentially a subset of the information in a full [`EvaluatedBlock`] and
/// its [`BlockAttributes`].
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct Evoxel {
    // TODO: Maybe we should convert to a smaller color format at this point?
    // These are frequently going to be copied into 32-bit texture color anyway.
    pub color: Rgba,
    pub selectable: bool,
    pub collision: BlockCollision,
}

impl Evoxel {
    /// The `Evoxel` value that would have resulted from using [`AIR`] in a recursive block.
    ///
    /// TODO: Write a test for that.
    pub const AIR: Self = Self {
        color: Rgba::TRANSPARENT,
        selectable: false,
        collision: BlockCollision::None,
    };

    /// Construct an [`Evoxel`] which represents the given evaluated block.
    ///
    /// This is the same operation as is used for each block/voxel in a [`Block::Recur`].
    pub fn from_block(block: &EvaluatedBlock) -> Self {
        Self {
            color: block.color,
            selectable: block.attributes.selectable,
            collision: block.attributes.collision,
        }
    }

    /// Construct the [`Evoxel`] that would have resulted from evaluating a voxel block
    /// with the given color and default attributes.
    pub const fn from_color(color: Rgba) -> Self {
        // Use the values from BlockAttributes's default for consistency.
        // Force constant promotion so that this doesn't look like a
        // feature(const_precise_live_drops) requirement
        const DA: &BlockAttributes = &BlockAttributes::default();
        Self {
            color,
            selectable: DA.selectable,
            collision: DA.collision,
        }
    }
}

/// Given the `resolution` of some recursive block occupying `cube`, transform `ray`
/// into an equivalent ray intersecting the recursive grid.
///
/// See also [`recursive_raycast`] for a raycast built on this.
// TODO: Decide whether this is good public API
#[inline]
pub(crate) fn recursive_ray(ray: Ray, cube: GridPoint, resolution: Resolution) -> Ray {
    Ray {
        origin: Point3::from_vec(
            (ray.origin - cube.map(FreeCoordinate::from)) * FreeCoordinate::from(resolution),
        ),
        direction: ray.direction,
    }
}

/// Given the `resolution` of some recursive block occupying `cube`, transform `ray`
/// into an equivalent ray intersecting the recursive grid, and start the raycast
/// through that block. This is equivalent to
///
/// ```skip
/// recursive_ray(ray, cube, resolution).cast().within_grid(Grid::for_block(resolution))
/// ```
// TODO: Decide whether this is good public API
#[inline]
pub(crate) fn recursive_raycast(ray: Ray, cube: GridPoint, resolution: Resolution) -> Raycaster {
    recursive_ray(ray, cube, resolution)
        .cast()
        .within_grid(Grid::for_block(resolution))
}

/// Notification when an [`EvaluatedBlock`] result changes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct BlockChange {
    /// I expect there _might_ be future uses for a set of flags of what changed;
    /// this helps preserve the option of adding them.
    _not_public: (),
}

impl BlockChange {
    #[allow(clippy::new_without_default)]
    pub fn new() -> BlockChange {
        BlockChange { _not_public: () }
    }
}

/// Construct a set of [`Block::Recur`] that form a miniature of the given `space`.
/// The returned [`Space`] contains each of the blocks; its coordinates will correspond to
/// those of the input, scaled down by `resolution`.
///
/// Returns [`SetCubeError::EvalBlock`] if the `Space` cannot be accessed, and
/// [`SetCubeError::TooManyBlocks`] if the dimensions would result in too many blocks.
///
/// TODO: add doc test for this
pub fn space_to_blocks(
    resolution: Resolution,
    attributes: BlockAttributes,
    space_ref: URef<Space>,
) -> Result<Space, SetCubeError> {
    let resolution_g: GridCoordinate = resolution.into();
    let source_grid = space_ref
        .try_borrow()
        // TODO: Not really the right error since this isn't actually an eval error.
        // Or is it close enough?
        .map_err(EvalBlockError::DataRefIs)?
        .grid();
    let destination_grid = source_grid.divide(resolution_g);

    let mut destination_space = Space::empty(destination_grid);
    destination_space.fill(destination_grid, move |cube| {
        Some(Block::Recur {
            attributes: attributes.clone(),
            offset: GridPoint::from_vec(cube.to_vec() * resolution_g),
            resolution,
            space: space_ref.clone(),
        })
    })?;
    Ok(destination_space)
}
