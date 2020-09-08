// Copyright 2020 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <http://opensource.org/licenses/MIT>.

//! Top-level game state container.

use indexmap::IndexSet;
use owning_ref::{OwningHandle, OwningRef, OwningRefMut};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::hash_map::{DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::camera::Camera;
use crate::space::{Space, SpaceStepInfo};

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum Name {
    Specific(String),
    Anonym(usize),
}
impl From<&str> for Name {
    fn from(value: &str) -> Self {
        Self::Specific(value.to_string())
    }
}

/// A collection of named objects which can refer to each other via `URef`. In the future,
/// it will enable multiple references, garbage collection, change notification, and
/// inter-object invariants.
pub struct Universe {
    spaces: HashMap<Name, URootRef<Space>>,
    cameras: HashMap<Name, URootRef<Camera>>,
    next_anonym: usize,
}

impl Universe {
    pub fn new() -> Self {
        Universe {
            spaces: HashMap::new(),
            // TODO: bodies so body-in-world stepping
            cameras: HashMap::new(),
            next_anonym: 0,
        }
    }

    // TODO: temporary shortcuts to be replaced with more nuance
    pub fn get_default_space(&self) -> URef<Space> {
        self.get(&"space".into()).unwrap()
    }
    pub fn get_default_camera(&self) -> URef<Camera> {
        self.get(&"camera".into()).unwrap()
    }

    /// Advance time.
    pub fn step(&mut self, timestep: Duration) -> (SpaceStepInfo, ()) {
        let mut space_info = SpaceStepInfo::default();
        for space in self.spaces.values() {
            space_info += space.borrow_mut().step(timestep);
        }
        for camera in self.cameras.values() {
            let _camera_info = camera.borrow_mut().step(timestep);
        }
        (space_info, ())
    }

    pub fn insert_anonymous<T>(&mut self, value: T) -> URef<T>
    where
        Self: UniverseIndex<T>,
    {
        // TODO: Names should not be strings, so these can be guaranteed unique.
        let name = Name::Anonym(self.next_anonym);
        self.next_anonym += 1;
        self.insert(name, value)
    }
}

/// Trait implemented once for each type of object that can be stored in a `Universe`.
pub trait UniverseIndex<T> {
    fn get(&self, name: &Name) -> Option<URef<T>>;
    fn insert(&mut self, name: Name, value: T) -> URef<T>;
}
impl UniverseIndex<Space> for Universe {
    fn get(&self, name: &Name) -> Option<URef<Space>> {
        self.spaces.get(name).map(URootRef::downgrade)
    }
    fn insert(&mut self, name: Name, value: Space) -> URef<Space> {
        // TODO: prohibit existing names under any type
        let root_ref = URootRef::new(name.clone(), value);
        let returned_ref = root_ref.downgrade();
        self.spaces.insert(name, root_ref);
        returned_ref
    }
}
impl UniverseIndex<Camera> for Universe {
    fn get(&self, name: &Name) -> Option<URef<Camera>> {
        self.cameras.get(name).map(URootRef::downgrade)
    }
    fn insert(&mut self, name: Name, value: Camera) -> URef<Camera> {
        // TODO: prohibit existing names under any type
        let root_ref = URootRef::new(name.clone(), value);
        let returned_ref = root_ref.downgrade();
        self.cameras.insert(name, root_ref);
        returned_ref
    }
}

impl Default for Universe {
    fn default() -> Self {
        Self::new()
    }
}

/// Type of a strong reference to an entry in a `Universe`. Defined to make types
/// parameterized with this somewhat less hairy.
type StrongEntryRef<T> = Rc<RefCell<UEntry<T>>>;

/// A reference from an object in a `Universe` to another.
///
/// If they are held by objects outside of the `Universe`, it is not guaranteed
/// that they will remain valid (in which case using the `URef` will panic).
/// To ensure an object does not vanish while operating on it, `.borrow()` it.
/// (TODO: Should there be an operation in the style of `Weak::upgrade`?)
#[derive(Debug)]
pub struct URef<T> {
    // TODO: We're going to want to either track reference counts or implement a garbage
    // collector for the graph of URefs. Reference counts would be an easy way to ensure
    // nothing is deleted while it is in use from a UI perspective.
    /// Reference to the object. Weak because we don't want to create reference cycles;
    /// the assumption is that the overall game system will keep the `Universe` alive
    /// and that `Universe` will ensure no entry goes away while referenced.
    weak_ref: Weak<RefCell<UEntry<T>>>,
    hash: u64,
}

impl<T: 'static> URef<T> {
    /// Borrow the value, in the sense of std::RefCell::borrow.
    pub fn borrow(&self) -> UBorrow<T> {
        UBorrow(OwningRef::new(OwningHandle::new(self.upgrade())).map(|entry| &entry.data))
    }

    /// Borrow the value mutably, in the sense of std::RefCell::borrow_mut.
    pub fn borrow_mut(&self) -> UBorrowMut<T> {
        UBorrowMut(
            OwningRefMut::new(OwningHandle::new_mut(self.upgrade()))
                .map_mut(|entry| &mut entry.data),
        )
    }

    fn upgrade(&self) -> Rc<RefCell<UEntry<T>>> {
        self.weak_ref.upgrade().expect("stale URef")
    }
}

/// `URef`s are compared by pointer equality: they are equal only if they refer to
/// the same mutable cell.
impl<T> PartialEq for URef<T> {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.weak_ref, &other.weak_ref)
    }
}
/// `URef`s are compared by pointer equality.
impl<T> Eq for URef<T> {}
impl<T> Hash for URef<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

/// Manual implementation of Clone that does not require T to be Clone.
impl<T> Clone for URef<T> {
    fn clone(&self) -> Self {
        URef {
            weak_ref: self.weak_ref.clone(),
            hash: self.hash,
        }
    }
}

/// A wrapper type for an immutably borrowed value from an `URef<T>`.
pub struct UBorrow<T: 'static>(
    OwningRef<OwningHandle<StrongEntryRef<T>, Ref<'static, UEntry<T>>>, T>,
);
/// A wrapper type for a mutably borrowed value from an `URef<T>`.
pub struct UBorrowMut<T: 'static>(
    OwningRefMut<OwningHandle<StrongEntryRef<T>, RefMut<'static, UEntry<T>>>, T>,
);
impl<T> Deref for UBorrow<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.deref()
    }
}
impl<T> Deref for UBorrowMut<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.deref()
    }
}
impl<T> DerefMut for UBorrowMut<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0.deref_mut()
    }
}

/// The data of an entry in a `Universe`.
#[derive(Debug)]
struct UEntry<T> {
    data: T,
    name: Name,
}

/// The unique reference to an entry in a `Universe` from that `Universe`.
/// Normal usage is via `URef` instead.
#[derive(Debug)]
struct URootRef<T> {
    strong_ref: StrongEntryRef<T>,
    /// Arbitrary hash code assigned to this entry's `URef`s.
    hash: u64,
}

impl<T> URootRef<T> {
    fn new(name: Name, initial_value: T) -> Self {
        // Grab whatever we can to make a random unique hash code, which isn't much.
        // Hopefully we're rarely creating many refs with the same name that will
        // be compared.
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);

        URootRef {
            strong_ref: Rc::new(RefCell::new(UEntry {
                data: initial_value,
                name,
            })),
            hash: hasher.finish(),
        }
    }

    /// Convert to `URef`.
    ///
    /// TODO: As we add graph analysis features, this will need additional arguments
    /// like where the ref is being held, and it will probably need to be renamed.
    fn downgrade(&self) -> URef<T> {
        URef {
            weak_ref: Rc::downgrade(&self.strong_ref),
            hash: self.hash,
        }
    }

    /// Borrow the value mutably, in the sense of std::RefCell::borrow_mut.
    fn borrow_mut(&self) -> UBorrowMut<T> {
        self.downgrade().borrow_mut()
    }
}

/// Mechanism for observing changes to objects. A `Notifier` delivers messages
/// to a set of listeners which implement some form of weak-reference semantics
/// to allow cleanup.
pub struct Notifier<M>
where
    M: Clone,
{
    listeners: Vec<Box<dyn Listener<M>>>,
}

impl<M> Notifier<M>
where
    M: Clone,
{
    pub fn new() -> Self {
        Self {
            listeners: Vec::new(),
        }
    }

    // TODO: Allow the caller to provide a filter on messages.
    pub fn listen<L: Listener<M> + 'static>(&mut self, listener: L) {
        self.cleanup();
        self.listeners.push(Box::new(listener));
    }

    pub fn notify(&self, message: M) {
        for listener in self.listeners.iter() {
            listener.receive(message.clone());
        }
    }

    /// Discard all dead weak pointers in `self.listeners`.
    fn cleanup(&mut self) {
        let mut i = 0;
        while i < self.listeners.len() {
            if self.listeners[i].alive() {
                i += 1;
            } else {
                self.listeners.swap_remove(i);
            }
        }
    }
}
impl<M> Default for Notifier<M>
where
    M: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

/// A receiver of messages which can indicate when it is no longer interested in
/// them (typically because the associated recipient has been dropped). Note that
/// a Listener must use interior mutability to store the message. As a Listener
/// may be called from various places, that mutability should in general be limited
/// to setting dirty flags or inserting into message queues — not triggering any
/// state changes of more general interest.
pub trait Listener<M> {
    fn receive(&self, message: M);
    fn alive(&self) -> bool;
}

/// A `Listener` which stores all the messages it receives, deduplicated.
pub struct Sink<M> {
    messages: Rc<RefCell<IndexSet<M>>>,
}
struct SinkListener<M> {
    weak_messages: Weak<RefCell<IndexSet<M>>>,
}
impl<M> Sink<M>
where
    M: Eq + Hash + Clone,
{
    pub fn new() -> Self {
        Self {
            messages: Rc::new(RefCell::new(IndexSet::new())),
        }
    }
    pub fn listener(&self) -> impl Listener<M> {
        SinkListener {
            weak_messages: Rc::downgrade(&self.messages),
        }
    }
}
/// As an Iterator, yields all messages currently waiting.
impl<M> Iterator for Sink<M>
where
    M: Eq + Hash + Clone,
{
    type Item = M;
    fn next(&mut self) -> Option<M> {
        self.messages.borrow_mut().pop()
    }
}
impl<M: Eq + Hash + Clone> Listener<M> for SinkListener<M> {
    fn receive(&self, message: M) {
        if let Some(cell) = self.weak_messages.upgrade() {
            cell.borrow_mut().insert(message);
        }
    }
    fn alive(&self) -> bool {
        self.weak_messages.upgrade().is_some()
    }
}
impl<M> Default for Sink<M>
where
    M: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

/// A `Listener` which only stores a single flag indicating if any messages were
/// received.
pub struct DirtyFlag {
    flag: Rc<Cell<bool>>,
}
pub struct DirtyFlagListener {
    weak_flag: Weak<Cell<bool>>,
}
impl DirtyFlag {
    pub fn new() -> Self {
        Self {
            flag: Rc::new(Cell::new(false)),
        }
    }
    pub fn listener<M>(&self) -> impl Listener<M> {
        DirtyFlagListener {
            weak_flag: Rc::downgrade(&self.flag),
        }
    }
    pub fn get_and_clear(&self) -> bool {
        self.flag.replace(false)
    }
}
impl<M> Listener<M> for DirtyFlagListener {
    fn receive(&self, _message: M) {
        if let Some(cell) = self.weak_flag.upgrade() {
            cell.set(true);
        }
    }
    fn alive(&self) -> bool {
        self.weak_flag.upgrade().is_some()
    }
}
impl Default for DirtyFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// A `Listener` which transforms messages before passing them on.
///
/// This may be used to drop uninteresting messages or reduce their granularity.
///
/// TODO: add doc test
pub struct Filter<MI, MO> {
    function: Box<dyn Fn(MI) -> Option<MO>>,
    target: Box<dyn Listener<MO>>,
}
impl<MI, MO> Listener<MI> for Filter<MI, MO> {
    fn receive(&self, message: MI) {
        if let Some(filtered_message) = (self.function)(message) {
            self.target.receive(filtered_message);
        }
    }
    fn alive(&self) -> bool {
        self.target.alive()
    }
}

/// Algorithm for deciding how to execute simulation and rendering frames.
/// Platform-independent; only returns decisions given provided information.
pub struct FrameClock {
    last_absolute_time: Option<Instant>,
    /// Whether there was a step and we should therefore draw a frame.
    /// TODO: This might go away in favor of actual dirty-notifications.
    render_dirty: bool,
    accumulated_step_time: Duration,
}

impl FrameClock {
    const STEP_LENGTH: Duration = Duration::from_micros(1_000_000 / 60);
    const ACCUMULATOR_CAP: Duration = Duration::from_millis(500);

    pub fn new() -> Self {
        Self {
            last_absolute_time: None,
            render_dirty: true,
            accumulated_step_time: Duration::default(),
        }
    }

    /// Advance the clock using a source of absolute time.
    ///
    /// This cannot be meaningfully used in combination with `request_frame()`.
    pub fn advance_to(&mut self, instant: Instant) {
        if let Some(last_absolute_time) = self.last_absolute_time {
            let delta = instant - last_absolute_time;
            self.accumulated_step_time += delta;
            self.cap_step_time();
        }
        self.last_absolute_time = Some(instant);
    }

    /// Reacts to a callback from the environment requesting drawing a frame ASAP if
    /// we're going to (i.e. requestAnimationFrame on the web). Drives the simulation
    /// clock based on this input (it will not advance if no requests are made).
    ///
    /// Returns whether a frame should actually be rendered now. The caller should also
    /// consult `should_step()` afterward to schedule game state steps.
    ///
    /// This cannot be meaningfully used in combination with `advance_to()`.
    #[must_use]
    pub fn request_frame(&mut self, time_since_last_frame: Duration) -> bool {
        let result = self.should_draw();
        self.did_draw();

        self.accumulated_step_time += time_since_last_frame;
        self.cap_step_time();

        result
    }

    pub fn should_draw(&self) -> bool {
        self.render_dirty
    }

    pub fn did_draw(&mut self) {
        self.render_dirty = false;
    }

    pub fn should_step(&self) -> bool {
        self.accumulated_step_time >= Self::STEP_LENGTH
    }

    pub fn step_length(&self) -> Duration {
        Self::STEP_LENGTH
    }

    pub fn did_step(&mut self) {
        self.accumulated_step_time -= Self::STEP_LENGTH;
        self.render_dirty = true;
    }

    fn cap_step_time(&mut self) {
        if self.accumulated_step_time > Self::ACCUMULATOR_CAP {
            self.accumulated_step_time = Self::ACCUMULATOR_CAP;
        }
    }
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockgen::make_some_blocks;

    #[test]
    fn uref_equality_is_pointer_equality() {
        let root_a = URootRef::new("space".into(), Space::empty_positive(1, 1, 1));
        let root_b = URootRef::new("space".into(), Space::empty_positive(1, 1, 1));
        let ref_a_1 = root_a.downgrade();
        let ref_a_2 = root_a.downgrade();
        let ref_b_1 = root_b.downgrade();
        assert_eq!(ref_a_1, ref_a_1, "reflexive eq");
        assert_eq!(ref_a_1, ref_a_2, "separately constructed are equal");
        assert!(ref_a_1 != ref_b_1, "not equal");
    }

    // TODO: more tests of the hairy reference logic

    #[test]
    fn insert_anonymous_makes_distinct_names() {
        let blocks = make_some_blocks(2);
        let mut u = Universe::new();
        let ref_a = u.insert_anonymous(Space::empty_positive(1, 1, 1));
        let ref_b = u.insert_anonymous(Space::empty_positive(1, 1, 1));
        ref_a.borrow_mut().set((0, 0, 0), &blocks[0]);
        ref_b.borrow_mut().set((0, 0, 0), &blocks[1]);
        assert!(ref_a != ref_b, "not equal");
        assert!(
            ref_a.borrow()[(0, 0, 0)] != ref_b.borrow()[(0, 0, 0)],
            "different values"
        );
    }

    #[test]
    fn notifier() {
        let mut cn: Notifier<u8> = Notifier::new();
        cn.notify(0);
        let mut sink = Sink::new();
        cn.listen(sink.listener());
        assert_eq!(None, sink.next());
        cn.notify(1);
        cn.notify(2);
        assert_eq!(Some(2), sink.next());
        assert_eq!(Some(1), sink.next());
        assert_eq!(None, sink.next());
    }
}
