//! [`Universe`], the top-level game-world container.
//!
//! ## Thread-safety
//!
//! [`Universe`], [`URef`], and their contents implement [`Send`] and [`Sync`],
//! such that it is possible to access a universe from multiple threads.
//! However, they do not (currently) provide any ability to wait to obtain a lock,
//! because there is not yet a policy about how one would avoid the possibility of
//! deadlock. Instead, you may only _attempt_ to acquire a lock, receiving an error if it
//! is already held. (TODO: Improve this.)
//!
//! For the time being, if you wish to use a [`Universe`] from multiple threads, you must
//! bring your own synchronization mechanisms to ensure that readers and writers do not
//! run at the same time.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use instant::Instant;

use crate::block::BlockDef;
use crate::character::Character;
use crate::space::{Space, SpaceStepInfo};
use crate::time::Tick;
use crate::transaction::Transaction as _;
use crate::util::{CustomFormat, StatusText};

// Note: Everything in `members` is either an impl, private, or intentionally public-in-private.
mod members;
pub(crate) use members::*;

mod universe_txn;
pub use universe_txn::*;

mod uref;
pub use uref::*;

mod visit;
pub use visit::*;

#[cfg(test)]
mod tests;

/// Name/key of an object in a [`Universe`].
///
/// Internally uses [`Arc`] to be cheap to clone. Might be interned in future versions.
#[allow(clippy::exhaustive_enums)]
#[derive(Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub enum Name {
    /// An explicitly set name.
    Specific(Arc<str>),

    /// An automatically assigned name.
    Anonym(usize),

    /// Not yet been assigned a name; this may be replaced with `Anonym` but not `Specific`.
    Pending,
}

impl From<&str> for Name {
    fn from(value: &str) -> Self {
        Self::Specific(value.into())
    }
}
impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Name::Specific(name) => write!(f, "'{name}'"),
            Name::Anonym(index) => write!(f, "[anonymous #{index}]"),
            Name::Pending => write!(f, "[pending anonymous]"),
        }
    }
}

/// Copiable unique (within this process) identifier for a [`Universe`].
///
/// Used to check whether [`URef`]s belong to particular [`Universe`]s.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct UniverseId(u64);

static UNIVERSE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

impl UniverseId {
    fn new() -> Self {
        Self(UNIVERSE_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// A collection of named objects which can refer to each other via [`URef`],
/// and which are simulated at the same time steps.
///
/// **Thread-safety caveat:** See the documentation on [avoiding deadlock].
///
/// [avoiding deadlock]: crate::universe#thread-safety
pub struct Universe {
    /// Storage of the actual members.
    tables: UniverseTables,

    id: UniverseId,
    /// Next number to assign to a [`Name::Anonym`].
    next_anonym: usize,
    /// Whether to run a garbage collection on the next step().
    /// This is set to true whenever a new member is inserted, which policy ensures
    /// that repeated insertion and dropping references cannot lead to unbounded growth
    /// as long as steps occur routinely.
    wants_gc: bool,
    /// Number of [`step()`]s which have occurred since this [`Universe`] was created.
    ///
    /// Note that this value is not serialized; thus, it is reset whenever “a saved game
    /// is loaded”. It should only be used for diagnostics or other non-persistent state.
    ///
    /// When a step is in progress, this is updated before stepping any members.
    ///
    /// TODO: This is not *currently* used for anything and should be removed if it never
    /// is. Its current reason to exist is for some not-yet-merged diagnostic logging.
    /// It may also serve a purpose when I refine the notion of simulation tick rate.
    ///
    /// [`step()`]: Universe::step
    session_step_time: u64,
}

impl Universe {
    /// Constructs an empty [`Universe`].
    pub fn new() -> Self {
        Universe {
            tables: UniverseTables::default(),
            id: UniverseId::new(),
            next_anonym: 0,
            wants_gc: false,
            session_step_time: 0,
        }
    }

    /// Returns a [`URef`] for the object in this universe with the given name,
    /// regardless of its type, or [`None`] if there is none.
    ///
    /// This is a dynamically-typed version of [`Universe::get()`].
    //
    // TODO: Find a useful way to implement this which does not require
    // boxing. Perhaps `URootRef` should implement `URefErased`? That would
    // change what `URef: Any` means, though. Perhaps `URootRef` should own
    // a prepared `URef` that it can return a reference to.
    pub fn get_any(&self, name: &Name) -> Option<Box<dyn URefErased>> {
        // TODO: Make this code macro-generated
        let UniverseTables {
            blocks,
            characters,
            spaces,
        } = &self.tables;

        if let Some(r) = blocks.get(name) {
            return Some(Box::new(r.downgrade()));
        }
        if let Some(r) = characters.get(name) {
            return Some(Box::new(r.downgrade()));
        }
        if let Some(r) = spaces.get(name) {
            return Some(Box::new(r.downgrade()));
        }
        None
    }

    /// Returns the character named `"character"`.
    /// This is currently assumed to be the “player character” for this universe.
    ///
    /// TODO: this is a temporary shortcut to be replaced with something with more nuance
    /// (e.g. we might have temporary characters for editing purposes, which are 'current'
    /// but not 'primary').
    pub fn get_default_character(&self) -> Option<URef<Character>> {
        self.get(&"character".into())
    }

    /// Returns a unique identifier for this particular [`Universe`] (within this memory space).
    ///
    /// It may be used to determine whether a given [`URef`] belongs to this universe or not.
    pub fn universe_id(&self) -> UniverseId {
        self.id
    }

    /// Advance time for all members.
    ///
    /// * `deadline` is when to stop computing flexible things such as light transport.
    pub fn step(&mut self, tick: Tick, deadline: Instant) -> UniverseStepInfo {
        let mut info = UniverseStepInfo::default();
        let start_time = Instant::now();

        if self.wants_gc {
            self.gc();
            self.wants_gc = false;
        }

        if !tick.paused() {
            self.session_step_time += 1;
        }

        let mut transactions = Vec::new();

        for space_root in self.tables.spaces.values() {
            let space_ref = space_root.downgrade();
            let (space_info, transaction) = space_ref
                .try_modify(|space| {
                    // TODO(time-budget): fairly divide deadline among members that need it.
                    // This implicitly implements an unfair "first wins" policy.
                    space.step(Some(&space_ref), tick, deadline)
                })
                .expect("space borrowed during universe.step()");
            transactions.push(transaction);
            info.space_step += space_info;
        }

        for character_root in self.tables.characters.values() {
            let character_ref = character_root.downgrade();
            let (_body_step_info, transaction) = character_ref
                .try_modify(|ch| ch.step(Some(&character_ref), tick))
                .expect("character borrowed during universe.step()");
            transactions.push(transaction);
        }

        // TODO: Quick hack -- we would actually like to execute non-conflicting transactions and skip conflicting ones...
        for t in transactions {
            if let Err(e) = t.execute(self, &mut drop) {
                // TODO: Need to report these failures back to the source
                // ... and perhaps in the UniverseStepInfo
                log::info!("Transaction failure: {}", e);
            }
        }

        info.computation_time = Instant::now().duration_since(start_time);
        info
    }

    /// Inserts a new object without giving it a specific name, and returns
    /// a reference to it.
    pub fn insert_anonymous<T>(&mut self, value: T) -> URef<T>
    where
        Self: UniverseOps<T>,
        T: UniverseMember,
    {
        self.insert(Name::Pending, value)
            .expect("shouldn't happen: insert_anonymous failed")
    }

    /// Translates a name for an object of type `T` into a [`URef`] for it, which
    /// allows borrowing the actual object.
    ///
    /// Returns [`None`] if no object exists for the name.
    pub fn get<T>(&self, name: &Name) -> Option<URef<T>>
    where
        Self: UniverseOps<T>,
        T: UniverseMember,
    {
        UniverseOps::get(self, name)
    }

    /// Inserts a new object with a specific name.
    ///
    /// Returns an error if the name is already in use.
    pub fn insert<T>(&mut self, name: Name, value: T) -> Result<URef<T>, InsertError>
    where
        Self: UniverseOps<T>,
        T: UniverseMember,
    {
        UniverseOps::insert(self, name, value)
    }

    /// Iterate over all of the objects of type `T`.
    /// Note that this includes anonymous objects.
    ///
    /// ```
    /// use all_is_cubes::block::{Block, BlockDef};
    /// use all_is_cubes::content::make_some_blocks;
    /// use all_is_cubes::universe::{Name, Universe, URef};
    ///
    /// let mut universe = Universe::new();
    /// let [block_1, block_2] = make_some_blocks();
    /// universe.insert(Name::from("b1"), BlockDef::new(block_1.clone()));
    /// universe.insert(Name::from("b2"), BlockDef::new(block_2.clone()));
    ///
    /// let mut found_blocks = universe.iter_by_type()
    ///     .map(|(name, value): (Name, URef<BlockDef>)| (name, Block::clone(&value.read().unwrap())))
    ///     .collect::<Vec<_>>();
    /// found_blocks.sort_by_key(|(name, _)| name.to_string());
    /// assert_eq!(
    ///     found_blocks,
    ///     vec![Name::from("b1"), Name::from("b2")].into_iter()
    ///         .zip(vec![block_1, block_2])
    ///         .collect::<Vec<_>>(),
    /// );
    /// ```
    pub fn iter_by_type<T>(&self) -> UniverseIter<'_, T>
    where
        Self: UniverseOps<T>,
        T: UniverseMember,
    {
        UniverseOps::iter_by_type(self)
    }

    /// Convert a possibly-[pending](Name::Pending) [`Name`] into a name that may be an
    /// actual name in this universe (which is always either [`Name::Specific`] or
    /// [`Name::Anonym`] if it succeeds).
    ///
    /// Fails if:
    ///
    /// * The name is already present.
    /// * The name is an [`Name::Anonym`] (which may not be pre-selected, only allocated).
    fn allocate_name(&mut self, proposed_name: &Name) -> Result<Name, InsertError> {
        // TODO: This logic is semi-duplicated in MemberTxn::check.
        // Resolve that by making all inserts happen via transactions, or by sharing
        // the code (this will need a "don't actually allocate anonym" mode).

        match proposed_name {
            Name::Specific(_) => {
                // Check that the name is not already used, under *any* type.
                if self.get_any(proposed_name).is_some() {
                    return Err(InsertError {
                        name: proposed_name.clone(),
                        kind: InsertErrorKind::AlreadyExists,
                    });
                }
                Ok(proposed_name.clone())
            }
            Name::Anonym(_) => Err(InsertError {
                name: proposed_name.clone(),
                kind: InsertErrorKind::InvalidName,
            }),
            Name::Pending => {
                let new_name = Name::Anonym(self.next_anonym);
                self.next_anonym += 1;

                assert!(
                    self.get_any(&new_name).is_none(),
                    "shouldn't happen: newly created anonym already in use"
                );
                Ok(new_name)
            }
        }
    }

    /// Delete a member.
    ///
    /// (Use [`UniverseTransaction::delete()`] as the public interface to this.)
    ///
    /// Returns whether the entry actually existed.
    pub(crate) fn delete(&mut self, name: &Name) -> bool {
        let UniverseTables {
            blocks,
            characters,
            spaces,
        } = &mut self.tables;

        blocks.remove(name).is_some()
            || characters.remove(name).is_some()
            || spaces.remove(name).is_some()
    }

    /// Delete all anonymous members which have no references to them.
    ///
    /// This may happen at any time during operations of the universe; calling this method
    /// merely ensures that it happens now and not earlier.
    pub fn gc(&mut self) {
        let UniverseTables {
            blocks,
            characters,
            spaces,
        } = &mut self.tables;

        // TODO: We need a real GC algorithm. For now, let's perform non-cyclic collection by
        // checking reference counts. If an entry has no weak references to its `Arc`, then
        // we know that it has no `URef`s.
        //
        // Besides not collecting cycles, this algorithm also has the flaw that it keeps
        // members around if there are `URef`s to them outside of the Universe, whereas the
        // preferred behavior, for consistency of the game logic, would be that they
        // go away at a time that is deterministic with respect to the simulation.
        gc_members(blocks);
        gc_members(characters);
        gc_members(spaces);
    }
}

impl fmt::Debug for Universe {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            tables,
            id: _,
            next_anonym: _,
            wants_gc: _,
            session_step_time,
        } = self;

        let mut ds = fmt.debug_struct("Universe");
        ds.field("session_step_time", &session_step_time);
        tables.fmt_members(&mut ds);
        ds.finish()
    }
}

/// Iterator type for [`Universe::iter_by_type`].
#[derive(Clone, Debug)]
pub struct UniverseIter<'u, T>(std::collections::btree_map::Iter<'u, Name, URootRef<T>>);
impl<'u, T> Iterator for UniverseIter<'u, T> {
    type Item = (Name, URef<T>);
    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|(name, root)| (name.clone(), root.downgrade()))
    }
}

impl Default for Universe {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors resulting from attempting to insert an object in a [`Universe`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct InsertError {
    /// The name given for the insertion.
    pub name: Name,
    /// The problem that was detected.
    pub kind: InsertErrorKind,
}

/// Specific problems with attempting to insert an object in a [`Universe`].
/// A component of [`InsertError`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InsertErrorKind {
    /// An object already exists with the proposed name.
    AlreadyExists,
    /// The proposed name may not be used.
    InvalidName,
    /// The provided [`URef`] does not have a value.
    Gone,
    /// The provided [`URef`] was already inserted into some universe.
    AlreadyInserted,
}

impl std::error::Error for InsertError {}
impl fmt::Display for InsertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { name, kind } = self;
        match kind {
            InsertErrorKind::AlreadyExists => {
                write!(f, "an object already exists with name {name}")
            }
            InsertErrorKind::InvalidName => {
                write!(f, "the name {name} may not be used in an insert operation")
            }
            InsertErrorKind::Gone => write!(f, "the URef {name} was already dead"),
            InsertErrorKind::AlreadyInserted => write!(f, "the object {name} is already inserted"),
        }
    }
}

/// Performance data returned by [`Universe::step`]. The exact contents of this structure
/// are unstable; use only `Debug` formatting to examine its contents unless you have
/// a specific need for one of the values.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct UniverseStepInfo {
    #[doc(hidden)]
    pub computation_time: Duration,
    space_step: SpaceStepInfo,
}
impl std::ops::AddAssign<UniverseStepInfo> for UniverseStepInfo {
    fn add_assign(&mut self, other: Self) {
        self.space_step += other.space_step;
    }
}
impl CustomFormat<StatusText> for UniverseStepInfo {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>, _: StatusText) -> fmt::Result {
        writeln!(
            fmt,
            "Step computation: {}",
            self.computation_time.custom_format(StatusText),
        )?;
        write!(fmt, "{}", self.space_step.custom_format(StatusText))?;
        Ok(())
    }
}

/// Helper for [`Universe::gc()`].
fn gc_members<T>(table: &mut Storage<T>) {
    let mut dead: Vec<Name> = Vec::new();
    for (name, root) in table.iter() {
        if root.weak_ref_count() == 0 {
            dead.push(name.clone());
        }
    }
    for name in dead {
        table.remove(&name);
    }
}

/// A subset of the [`URef`]s in one universe.
///
/// May be serialized as if it was a [`Universe`].
///
/// This structure is not currently exposed because it is a helper for
/// `all_is_cubes_port::ExportSet` and doesn't play a role in the API itself.
#[doc(hidden)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[allow(clippy::exhaustive_structs)]
pub struct PartialUniverse {
    // TODO: design API that doesn't rely on making these public, but still allows
    // exports to be statically exhaustive.
    pub blocks: Vec<URef<BlockDef>>,
    pub characters: Vec<URef<Character>>,
    pub spaces: Vec<URef<Space>>,
}

impl PartialUniverse {
    pub fn all_of(universe: &Universe) -> Self {
        Self {
            blocks: universe.iter_by_type().map(|(_, r)| r).collect(),
            characters: universe.iter_by_type().map(|(_, r)| r).collect(),
            spaces: universe.iter_by_type().map(|(_, r)| r).collect(),
        }
    }

    /// Select only the given members.
    pub fn from_set<T>(members: impl IntoIterator<Item = URef<T>>) -> Self
    where
        T: UniverseMember,
        Self: PartialUniverseOps<T>,
    {
        <Self as PartialUniverseOps<T>>::from_set(members)
    }

    #[doc(hidden)]
    pub fn count(&self) -> usize {
        let Self {
            blocks,
            characters,
            spaces,
        } = self;
        blocks.len() + characters.len() + spaces.len()
    }
}
