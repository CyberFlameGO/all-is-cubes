use std::collections::{hash_map::Entry::*, HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, Weak};

use cgmath::Point3;
use fnv::{FnvHashMap, FnvHashSet};
use indoc::indoc;
use instant::{Duration, Instant};

use crate::block::{EvaluatedBlock, Resolution};
use crate::camera::{Camera, Flaws};
use crate::chunking::{cube_to_chunk, point_to_chunk, ChunkChart, ChunkPos, OctantMask};
use crate::listen::{Listen as _, Listener};
use crate::math::{Aab, FreeCoordinate, Geometry as _, GridCoordinate, GridPoint};
use crate::mesh::{
    BlockMesh, BlockMeshProvider, GfxVertex, LineVertex, MeshOptions, SpaceMesh, TextureAllocator,
    TextureTile,
};
use crate::space::{BlockIndex, Space, SpaceChange};
use crate::universe::URef;
use crate::util::{CustomFormat, StatusText, TimeStats};

mod blocks {}

mod chunk;
pub use chunk::*;

/// If true, enables reporting chunk update timing at [`log::trace`] level.
const LOG_CHUNK_UPDATES: bool = false;

/// The large-scale analogue of [`SpaceMesh`]: subdivides a [`Space`] into
/// [chunks](crate::chunking) which are individually recomputed as the space changes or
/// its contained blocks do.
///
/// Each chunk, a [`ChunkMesh`], owns a data value of type `D`, which is
/// initialized using `D::default()`. This value may be a reference to a corresponding
/// GPU buffer, for example. It will usually need to be an [`Option`] of something.
#[derive(Debug)]
pub struct ChunkedSpaceMesh<D, Vert, Tex, const CHUNK_SIZE: GridCoordinate>
where
    Tex: TextureAllocator,
{
    space: URef<Space>,

    /// Dirty flags listening to `space`.
    todo: Arc<Mutex<CsmTodo<CHUNK_SIZE>>>,

    block_meshes: VersionedBlockMeshes<Vert, Tex::Tile>,

    /// Invariant: the set of present chunks (keys here) is the same as the set of keys
    /// in `todo.read().unwrap().chunks`.
    chunks: FnvHashMap<ChunkPos<CHUNK_SIZE>, ChunkMesh<D, Vert, Tex, CHUNK_SIZE>>,

    /// Resized as needed upon each [`Self::update_blocks_and_some_chunks()`].
    chunk_chart: ChunkChart<CHUNK_SIZE>,

    /// The chunk in which the last [`Camera`] provided is located.
    view_chunk: ChunkPos<CHUNK_SIZE>,

    /// Whether, on the previous frame, we did not finish updating all visible chunks.
    ///
    /// If so, then we prioritize adding new chunks over updating existing ones,
    /// because blank world is a worse outcome than slightly stale world.
    did_not_finish_chunks: bool,

    /// The [`MeshOptions`] specified by the last [`Camera`] provided.
    last_mesh_options: Option<MeshOptions>,

    /// Most recent time at which we reset to no data.
    zero_time: Instant,
    /// Earliest time prior to `zero_time` at which we finished everything in the queues.
    complete_time: Option<Instant>,
}

impl<D, Vert, Tex, const CHUNK_SIZE: GridCoordinate> ChunkedSpaceMesh<D, Vert, Tex, CHUNK_SIZE>
where
    D: Default,
    Vert: GfxVertex<TexPoint = <<Tex as TextureAllocator>::Tile as TextureTile>::Point> + PartialEq,
    Tex: TextureAllocator,
    Tex::Tile: PartialEq,
{
    pub fn new(space: URef<Space>) -> Self {
        todo!()
    }

    /// Returns a reference to the [`Space`] this watches.
    pub fn space(&self) -> &URef<Space> {
        &self.space
    }

    /// Returns a [`ChunkChart`] for the view distance used by the most recent
    /// [`Self::update_blocks_and_some_chunks`].
    pub fn chunk_chart(&self) -> &ChunkChart<CHUNK_SIZE> {
        &self.chunk_chart
    }

    /// Iterate over the [`ChunkMesh`]es of all chunks, in arbitrary order.
    ///
    /// Empty chunks are included; in particular, this will iterate over every `D` value
    /// owned by this [`ChunkedSpaceMesh`].
    ///
    /// TODO: Define the order
    pub fn iter_chunks(&self) -> impl Iterator<Item = &ChunkMesh<D, Vert, Tex, CHUNK_SIZE>> {
        [].into_iter()
    }

    /// Iterate over the [`ChunkMesh`]es that are in view from the given camera,
    /// in front-to-back order. (Use `.rev()` to iterate in back-to-front order.)
    ///
    /// Uses `camera`'s position, rotation, and options to decide which chunks to return.
    pub fn iter_in_view<'a>(
        &'a self,
        camera: &'a Camera,
    ) -> impl Iterator<Item = &'a ChunkMesh<D, Vert, Tex, CHUNK_SIZE>> + DoubleEndedIterator + 'a
    {
        [].into_iter()
    }

    /// Retrieves a [`ChunkMesh`] for the specified chunk position, if one exists.
    ///
    /// Call this while drawing, after [`Self::update_blocks_and_some_chunks`]
    /// has updated/created chunks.
    pub fn chunk(
        &self,
        position: ChunkPos<CHUNK_SIZE>,
    ) -> Option<&ChunkMesh<D, Vert, Tex, CHUNK_SIZE>> {
        todo!()
    }

    /// Recompute meshes of all blocks that need it, and the nearest chunks that need it.
    ///
    /// * `camera`'s view position is used to choose what to update and for depth
    ///    ordering; its graphics options are used for triangulation and view distance.
    /// * `deadline` is the approximate time at which this should stop.
    /// * `chunk_render_updater` is called for every re-meshed or depth-sorted chunk.
    ///
    /// Returns performance information and the chunk the camera is located in.
    ///
    /// TODO: The updaters should be changed to be one value instead of two, so that
    /// they can share mutable state if needed. This will benefit the wgpu renderer.
    pub fn update_blocks_and_some_chunks<F>(
        &mut self,
        camera: &Camera,
        block_texture_allocator: &Tex,
        deadline: Instant,
        mut chunk_render_updater: F,
    ) -> CsmUpdateInfo
    where
        F: FnMut(ChunkMeshUpdate<'_, D, Vert, Tex::Tile, CHUNK_SIZE>),
    {
        todo!()
    }

    /// Returns the chunk in which the camera from the most recent
    /// [`Self::update_blocks_and_some_chunks`] was located.
    /// This may be used as the origin point to iterate over chunks in view.
    pub fn view_chunk(&self) -> ChunkPos<CHUNK_SIZE> {
        self.view_chunk
    }

    /// Produces lines that visualize the boundaries of visible nonempty chunks.
    #[doc(hidden)] // TODO: good public API?
    pub fn chunk_debug_lines(&self, camera: &Camera, output: &mut impl Extend<LineVertex>) {
        todo!()
    }
}

/// Performance info from a [`ChunkedSpaceMesh`]'s per-frame update.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct CsmUpdateInfo {
    /// Flaws detected during update.
    /// Note that this does not include mesh flaws; the caller must gather those when
    /// drawing the chunks.
    pub flaws: Flaws,
    pub total_time: Duration,
    /// Time spent on gathering information before starting the chunk scan.
    pub prep_time: Duration,
    /// Time spent on traversing chunks in view this frame,
    /// excluding the actual chunk mesh generation operations.
    pub chunk_scan_time: Duration,
    /// Time spent on building chunk meshes this frame.
    pub chunk_mesh_generation_times: TimeStats,
    /// Time spent on `chunk_mesh_updater` callbacks this frame.
    pub chunk_mesh_callback_times: TimeStats,
    depth_sort_time: Option<Duration>,
    /// Time spent on building block meshes this frame.
    pub block_updates: TimeStats,

    /// Number of chunks that currently exist.
    pub chunk_count: usize,
    /// Total in-memory size of chunk data (not counting [`ChunkMesh::render_data`]).
    pub chunk_total_cpu_byte_size: usize,
}

impl CustomFormat<StatusText> for CsmUpdateInfo {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>, _: StatusText) -> fmt::Result {
        let CsmUpdateInfo {
            flaws,
            total_time: _,
            prep_time,
            chunk_scan_time,
            chunk_mesh_generation_times,
            chunk_mesh_callback_times,
            depth_sort_time,
            block_updates,
            chunk_count,
            chunk_total_cpu_byte_size,
        } = self;
        write!(
            fmt,
            indoc! {"
                Space prep     {prep_time}       Mesh flaws: {flaws}
                Block mesh gen {block_updates}
                Chunk scan     {chunk_scan_time}
                      mesh gen {chunk_mesh_generation_times}
                      upload   {chunk_mesh_callback_times}
                      depthsort {depth_sort_time}
                Mem: {chunk_mib} MiB for {chunk_count} chunks\
            "},
            flaws = flaws,
            prep_time = prep_time.custom_format(StatusText),
            block_updates = block_updates,
            chunk_scan_time = chunk_scan_time.custom_format(StatusText),
            chunk_mesh_generation_times = chunk_mesh_generation_times,
            chunk_mesh_callback_times = chunk_mesh_callback_times,
            depth_sort_time = depth_sort_time
                .unwrap_or(Duration::ZERO)
                .custom_format(StatusText),
            chunk_mib = chunk_total_cpu_byte_size / (1024 * 1024),
            chunk_count = chunk_count,
        )
    }
}

#[derive(Debug)]
struct VersionedBlockMeshes<Vert, Tile> {
    /// Indices of this vector are block IDs in the Space.
    meshes: Vec<VersionedBlockMesh<Vert, Tile>>,

    last_version_counter: NonZeroU32,
}

impl<Vert, Tile> VersionedBlockMeshes<Vert, Tile>
where
    Vert: GfxVertex<TexPoint = <Tile as TextureTile>::Point> + PartialEq,
    Tile: TextureTile + PartialEq,
{
    fn new() -> Self {
        Self {
            meshes: Vec::new(),
            last_version_counter: NonZeroU32::new(u32::MAX).unwrap(),
        }
    }

    /// Discard all meshes.
    /// Use this to ensure that in case of “everything changes” we don't store
    /// extra data.
    fn clear(&mut self) {
        self.meshes.clear();
    }

    /// Update block meshes based on the given [`Space`].
    ///
    /// After this method returns, `self.meshes.len()` will
    /// always equal `space.block_data().len()`. It may not be fully updated yet, but
    /// it will be the correct length.
    ///
    /// TODO: Missing handling for `mesh_options` changing.
    fn update<A>(
        &mut self,
        todo: &mut FnvHashSet<BlockIndex>,
        space: &Space,
        block_texture_allocator: &A,
        mesh_options: &MeshOptions,
        deadline: Instant,
    ) -> TimeStats
    where
        A: TextureAllocator<Tile = Tile>,
    {
        if todo.is_empty() {
            // Don't increment the version counter if we don't need to.
            return TimeStats::default();
        }

        // Bump version number.
        self.last_version_counter = match self.last_version_counter.get().checked_add(1) {
            None => NonZeroU32::new(1).unwrap(),
            Some(n) => NonZeroU32::new(n).unwrap(),
        };
        let current_version_number = BlockMeshVersion::Numbered(self.last_version_counter);

        let block_data = space.block_data();

        // Synchronize the mesh storage vector's length.
        {
            let old_len = self.meshes.len();
            let new_len = block_data.len();
            if old_len > new_len {
                self.meshes.truncate(new_len);
            } else {
                // Increase length, and initialize the new elements.
                // This must be done quickly, so that we do not have a hiccup when initializing
                // from a space with many blocks.

                let mut fast_options = mesh_options.clone();
                fast_options.ignore_voxels = true;

                self.meshes.reserve(new_len);
                for bd in &block_data[self.meshes.len()..new_len] {
                    let evaluated = bd.evaluated();
                    self.meshes
                        .push(if evaluated.resolution() > Resolution::R1 {
                            // If the block has voxels, generate a placeholder mesh,
                            // marked as not-ready so it will be replaced eventually.
                            VersionedBlockMesh {
                                mesh: BlockMesh::new(
                                    evaluated,
                                    block_texture_allocator,
                                    &fast_options,
                                ),
                                version: BlockMeshVersion::NotReady,
                            }
                        } else {
                            // If the block does not have voxels, then we can just generate the
                            // final mesh as quick as the placeholder.
                            VersionedBlockMesh {
                                mesh: BlockMesh::new(
                                    evaluated,
                                    block_texture_allocator,
                                    mesh_options,
                                ),
                                version: current_version_number,
                            }
                        });
                }
            }
        }

        // Update individual meshes.
        let mut last_start_time = Instant::now();
        let mut stats = TimeStats::default();
        while last_start_time < deadline && !todo.is_empty() {
            let index: BlockIndex = todo.iter().next().copied().unwrap();
            todo.remove(&index);
            let index: usize = index.into();

            let bd = &block_data[index];
            let new_evaluated_block: &EvaluatedBlock = bd.evaluated();
            let current_mesh_entry: &mut VersionedBlockMesh<_, _> = &mut self.meshes[index];

            // TODO: Consider re-introducing approximate cost measurement
            // to hit the deadline better.
            // cost += match &new_evaluated_block.voxels {
            //     Some(voxels) => voxels.bounds().volume(),
            //     None => 1,
            // };

            if current_mesh_entry
                .mesh
                .try_update_texture_only(new_evaluated_block)
            {
                // Updated the texture in-place. No need for mesh updates.
            } else {
                // Compute a new mesh.
                // TODO: Try using BlockMesh::compute() to reuse allocations.
                // The catch is that then we will no longer be able to compare the new mesh
                // to the existing mesh below, so this won't be a pure win.
                let new_block_mesh =
                    BlockMesh::new(new_evaluated_block, block_texture_allocator, mesh_options);

                // Only invalidate the chunks if we actually have different data.
                // Note: This comparison depends on such things as the definition of PartialEq
                // for Tex::Tile.
                // TODO: We don't currently make use of this optimally because textures are never
                // reused, except in the case of texture-only updates handled above.
                // (If they were, we'd need to consider what we want to do about stale chunks with
                // updated texture tiles, which might have geometry gaps or otherwise be obviously
                // inconsistent.)
                if new_block_mesh != current_mesh_entry.mesh
                    || current_mesh_entry.version == BlockMeshVersion::NotReady
                {
                    *current_mesh_entry = VersionedBlockMesh {
                        mesh: new_block_mesh,
                        version: current_version_number,
                    };
                } else {
                    // The new mesh is identical to the old one (which might happen because
                    // interior voxels or non-rendered attributes were changed), so don't invalidate
                    // the chunks.
                }
            }
            let duration = stats.record_consecutive_interval(&mut last_start_time, Instant::now());
            if duration > Duration::from_millis(4) {
                log::trace!(
                    "Block mesh took {}: {:?} {:?}",
                    duration.custom_format(StatusText),
                    new_evaluated_block.attributes.display_name,
                    bd.block(),
                );
            }
        }

        stats
    }
}

impl<'a, Vert, Tile> BlockMeshProvider<'a, Vert, Tile> for &'a VersionedBlockMeshes<Vert, Tile> {
    fn get(&mut self, index: BlockIndex) -> Option<&'a BlockMesh<Vert, Tile>> {
        Some(&self.meshes.get(usize::from(index))?.mesh)
    }
}

/// Entry in [`VersionedBlockMeshes`].
#[derive(Debug)]
struct VersionedBlockMesh<Vert, Tile> {
    mesh: BlockMesh<Vert, Tile>,
    /// Version ID used to track whether chunks have stale block meshes (ones that don't
    /// match the current definition of that block-index in the space).
    version: BlockMeshVersion,
}

/// Together with a [`BlockIndex`], uniquely identifies a block mesh.
/// Used to determine when chunk meshes need updating.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BlockMeshVersion {
    /// The block mesh hasn't been computed yet and this is the placeholder mesh.
    /// Special because it's never assigned as a "good" version number.
    NotReady,
    /// A specific version.
    /// u32 is sufficient size because we are extremely unlikely to wrap around u32 space
    /// in the course of a single batch of updates unless we're perpetually behind.
    Numbered(NonZeroU32),
}

/// Stores a [`SpaceMesh`] covering one chunk of a [`Space`], caller-provided rendering
/// data, and incidentals.
#[derive(Debug, Eq, PartialEq)]
pub struct ChunkMesh<D, Vert, Tex, const CHUNK_SIZE: GridCoordinate>
where
    Tex: TextureAllocator,
{
    position: ChunkPos<CHUNK_SIZE>,
    mesh: SpaceMesh<Vert, Tex::Tile>,
    pub render_data: D,
    block_dependencies: Vec<(BlockIndex, BlockMeshVersion)>,

    /// Toggled whenever the mesh is updated. Value is arbitrary (this is a looping
    /// 2-state counter).
    update_debug: bool,
}

/// Provides mutable access to the render data of type `D` in a [`ChunkMesh`].
///
/// This struct is provided to the callbacks of
/// [`ChunkedSpaceMesh::update_blocks_and_some_chunks()`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ChunkMeshUpdate<'a, D, V, T, const CHUNK_SIZE: GridCoordinate> {
    pub position: ChunkPos<CHUNK_SIZE>,
    pub mesh: &'a SpaceMesh<V, T>,
    pub render_data: &'a mut D,
    /// Whether *only* the indices need to be copied (and their length has not changed).
    pub indices_only: bool,
}

/// [`ChunkedSpaceMesh`]'s set of things that need recomputing.
#[derive(Debug, Default)]
struct CsmTodo<const CHUNK_SIZE: GridCoordinate> {
    all_blocks_and_chunks: bool,
    // TODO: Benchmark using a BitVec instead.
    blocks: FnvHashSet<BlockIndex>,
    /// Membership in this table indicates that the chunk *exists;* todos for chunks
    /// outside of the view area are not tracked.
    chunks: FnvHashMap<ChunkPos<CHUNK_SIZE>, ChunkTodo>,
}

impl<const CHUNK_SIZE: GridCoordinate> CsmTodo<CHUNK_SIZE> {
    fn initially_dirty() -> Self {
        Self {
            all_blocks_and_chunks: true,
            blocks: HashSet::default(),
            chunks: HashMap::default(),
        }
    }

    fn modify_block_and_adjacent<F>(&mut self, cube: GridPoint, mut f: F)
    where
        F: FnMut(&mut ChunkTodo),
    {
        // Mark adjacent blocks to account for opaque faces hiding adjacent
        // blocks' faces. We don't need to bother with the current block since
        // the adjacent chunks will always include it (presuming that the chunk
        // size is greater than 1).
        for axis in 0..3 {
            for offset in &[-1, 1] {
                let mut adjacent = cube;
                adjacent[axis] += offset;
                if let Some(chunk) = self.chunks.get_mut(&cube_to_chunk(adjacent)) {
                    f(chunk);
                }
            }
        }
    }
}

/// [`Listener`] adapter for [`CsmTodo`].
#[derive(Clone, Debug)]
struct TodoListener<const CHUNK_SIZE: GridCoordinate>(Weak<Mutex<CsmTodo<CHUNK_SIZE>>>);

impl<const CHUNK_SIZE: GridCoordinate> Listener<SpaceChange> for TodoListener<CHUNK_SIZE> {
    fn receive(&self, message: SpaceChange) {
        if let Some(cell) = self.0.upgrade() {
            if let Ok(mut todo) = cell.lock() {
                match message {
                    SpaceChange::EveryBlock => {
                        todo.all_blocks_and_chunks = true;
                        todo.blocks.clear();
                        todo.chunks.clear();
                    }
                    SpaceChange::Block(p) => {
                        todo.modify_block_and_adjacent(p, |chunk_todo| {
                            chunk_todo.recompute_mesh = true;
                        });
                    }
                    SpaceChange::Lighting(_p) => {
                        // Meshes are not affected by light
                    }
                    SpaceChange::Number(index) => {
                        if !todo.all_blocks_and_chunks {
                            todo.blocks.insert(index);
                        }
                    }
                    SpaceChange::BlockValue(index) => {
                        if !todo.all_blocks_and_chunks {
                            todo.blocks.insert(index);
                        }
                    }
                }
            }
        }
    }

    fn alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}
