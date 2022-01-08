// Copyright 2020-2022 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <https://opensource.org/licenses/MIT>.

//! Components for "apps", or game clients: user interface and top-level state.

use std::fmt::{self, Display};
use std::future::Future;
use std::mem;
use std::sync::mpsc::{self, TryRecvError};
use std::task::{Context, Poll};

use cgmath::{One, Point2};
use futures_core::future::BoxFuture;
use futures_task::noop_waker_ref;

use crate::camera::{Camera, GraphicsOptions, Viewport};
use crate::character::{cursor_raycast, Character, Cursor};
use crate::inv::{Tool, ToolError, ToolInput};
use crate::listen::{DirtyFlag, ListenableCell, ListenableCellWithLocal, ListenableSource};
use crate::math::FreeCoordinate;
use crate::space::Space;
use crate::transaction::Transaction;
use crate::universe::{URef, Universe, UniverseStepInfo};
use crate::util::{CustomFormat, StatusText};
use crate::vui::Vui;

mod input;
pub use input::*;

mod time;
pub use time::*;

/// Everything that a game application needs regardless of platform.
///
/// Once we have multiplayer / client-server support, this will become the client-side
/// structure.
pub struct AllIsCubesAppState {
    /// Determines the timing of simulation and drawing. The caller must arrange
    /// to advance time in the clock.
    pub frame_clock: FrameClock,

    /// Handles (some) user input. The caller must provide input events/state;
    /// `AllIsCubesAppState` will handle calling [`InputProcessor::apply_input`].
    pub input_processor: InputProcessor,

    graphics_options: ListenableCell<GraphicsOptions>,

    game_universe: Universe,
    game_character: ListenableCellWithLocal<Option<URef<Character>>>,

    /// If present, a future that should be polled to produce a new [`Universe`]
    /// to replace `self.game_universe`. See [`Self::set_universe_async`].
    game_universe_in_progress: Option<BoxFuture<'static, Result<Universe, ()>>>,

    paused: ListenableCell<bool>,

    ui: Vui,

    /// Messages for controlling the state that aren't via [`InputProcessor`].
    ///
    /// TODO: This is originally a quick kludge to make onscreen UI buttons work.
    /// Not sure whether it is a good strategy overall.
    control_channel: mpsc::Receiver<ControlMessage>,

    /// Last cursor raycast result.
    /// TODO: This needs to handle clicking on the HUD and thus explicitly point into
    /// one of two different spaces.
    cursor_result: Option<Cursor>,

    last_step_info: UniverseStepInfo,
    // When adding fields, remember to update the `Debug` impl.
}

impl fmt::Debug for AllIsCubesAppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllIsCubesAppState")
            .field("frame_clock", &self.frame_clock)
            .field("input_processor", &self.input_processor)
            .field("graphics_options", &self.graphics_options)
            .field("game_universe", &self.game_universe)
            .field("game_character", &self.game_character)
            .field(
                "game_universe_in_progress",
                &self.game_universe_in_progress.as_ref().map(|_| "..."),
            )
            .field("paused", &self.paused)
            .field("ui", &self.ui)
            .field("cursor_result", &self.cursor_result)
            .field("last_step_info", &self.last_step_info)
            .finish_non_exhaustive()
    }
}

impl AllIsCubesAppState {
    /// Construct a new `AllIsCubesAppState` with a new [`Universe`] from the given
    /// template.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let game_universe = Universe::new();
        let game_character = ListenableCellWithLocal::new(None);
        let input_processor = InputProcessor::new();
        let paused = ListenableCell::new(false);
        let (control_send, control_recv) = mpsc::sync_channel(100);

        Self {
            ui: Vui::new(
                &input_processor,
                game_character.as_source(),
                paused.as_source(),
                control_send,
            ),

            frame_clock: FrameClock::new(),
            input_processor,
            graphics_options: ListenableCell::new(GraphicsOptions::default()),
            game_character,
            game_universe,
            game_universe_in_progress: None,
            paused,
            control_channel: control_recv,
            cursor_result: None,
            last_step_info: UniverseStepInfo::default(),
        }
    }

    /// Returns a source for the [`Character`] that should be shown to the user.
    pub fn character(&self) -> ListenableSource<Option<URef<Character>>> {
        self.game_character.as_source()
    }

    /// Replace the game universe, such as on initial startup or because the player
    /// chose to load a new one.
    pub fn set_universe(&mut self, u: Universe) {
        // Clear any previous set_universe_async.
        self.game_universe_in_progress = None;

        self.game_universe = u;
        self.game_character
            .set(self.game_universe.get_default_character());
    }

    /// Perform [`Self::set_universe`] on the result of the provided future when it
    /// completes.
    ///
    /// This is intended to be used for simultaneously initializing the UI and universe.
    /// Later upgrades might might add a loading screen.
    ///
    /// The future will be cancelled if [`Self::set_universe_async`] or
    /// [`Self::set_universe`] is called before it completes.
    /// Currently, the future is polled once per frame unconditionally.
    ///
    /// If the future returns `Err`, then the current universe is not replaced. There is
    /// not any mechanism to display an error message; that must be done separately.
    pub fn set_universe_async<F>(&mut self, future: F)
    where
        F: Future<Output = Result<Universe, ()>> + Send + 'static,
    {
        self.game_universe_in_progress = Some(Box::pin(future));
    }

    /// Returns a mutable reference to the [`Universe`].
    ///
    /// Note: Replacing the universe will not update the UI and character state.
    /// Use [`Self::set_universe`] instead.
    pub fn universe_mut(&mut self) -> &mut Universe {
        &mut self.game_universe
    }

    pub fn ui_space(&self) -> &URef<Space> {
        self.ui.current_space()
    }

    pub fn graphics_options(&self) -> ListenableSource<GraphicsOptions> {
        self.graphics_options.as_source()
    }

    pub fn graphics_options_mut(&self) -> &ListenableCell<GraphicsOptions> {
        &self.graphics_options
    }

    /// Steps the universe if the `FrameClock` says it's time to do so.
    /// Always returns info for the last step even if multiple steps were taken.
    ///
    /// Also applies input from the control channel. TODO: Should that be separate?
    pub fn maybe_step_universe(&mut self) -> Option<UniverseStepInfo> {
        loop {
            match self.control_channel.try_recv() {
                Ok(msg) => match msg {
                    ControlMessage::TogglePause => {
                        self.paused.set(!*self.paused.get());
                    }
                    ControlMessage::ToggleMouselook => {
                        self.input_processor.toggle_mouselook_mode();
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Lack of whatever control sources is non-fatal.
                }
            }
        }

        if let Some(future) = self.game_universe_in_progress.as_mut() {
            match future
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()))
            {
                Poll::Pending => {}
                Poll::Ready(result) => {
                    self.game_universe_in_progress = None;
                    match result {
                        Ok(universe) => {
                            self.set_universe(universe);
                        }
                        Err(()) => {
                            // No error reporting, for now; let it be the caller's resposibility
                            // (which we indicate by making the error type be ()).
                            // There should be something, but it's not clear what; perhaps
                            // it will become clearer as the UI gets fleshed out.
                        }
                    }
                }
            }
        }

        let mut result = None;
        // TODO: Catch-up implementation should probably live in FrameClock.
        for _ in 0..FrameClock::CATCH_UP_STEPS {
            if self.frame_clock.should_step() {
                let base_tick = self.frame_clock.tick();
                let game_tick = if *self.paused.get() {
                    base_tick.pause()
                } else {
                    base_tick
                };
                self.frame_clock.did_step();

                if let Some(character_ref) = self.game_character.borrow() {
                    self.input_processor.apply_input(
                        InputTargets {
                            universe: Some(&mut self.game_universe),
                            character: Some(character_ref),
                            paused: Some(&self.paused),
                            graphics_options: Some(&self.graphics_options),
                        },
                        game_tick,
                    );
                }
                self.input_processor.step(game_tick);

                let mut info = self.game_universe.step(game_tick);

                info += self.ui.step(base_tick);

                self.last_step_info = info.clone();
                result = Some(info)
            }
        }
        result
    }

    /// Call this once per frame to update the cursor raycast.
    ///
    /// TODO: bad API; revisit general cursor handling logic.
    /// We'd like to not have too much dependencies on the rendering, but also
    /// not obligate each platform/renderer layer to have too much boilerplate.
    pub fn update_cursor(&mut self, cameras: &StandardCameras) {
        self.cursor_result = self
            .input_processor
            .cursor_ndc_position()
            .and_then(|ndc_pos| cameras.project_cursor(ndc_pos));
    }

    pub fn cursor_result(&self) -> &Option<Cursor> {
        &self.cursor_result
    }

    /// Handle a mouse-click event, at the position specified by the last
    /// [`Self::update_cursor()`].
    ///
    /// TODO: Clicks should be passed through `InputProcessor` instead of being an entirely separate path.
    pub fn click(&mut self, button: usize) {
        match self.click_impl(button) {
            Ok(()) => {}
            Err(e) => self.ui.show_tool_error(e),
        }
    }

    /// Implementation of click interpretation logic, called by [`Self::click`].
    /// TODO: This function needs tests.
    fn click_impl(&mut self, button: usize) -> Result<(), ToolError> {
        let cursor_space = self.cursor_result.as_ref().map(|c| &c.space);
        if cursor_space == Some(self.ui_space()) {
            // Clicks on UI use `Tool::Activate`.
            // TODO: We'll probably want to distinguish buttons eventually.
            // TODO: It should be easier to use a tool
            let transaction = Tool::Activate.use_immutable_tool(&ToolInput {
                cursor: self.cursor_result.clone(),
                character: None,
            })?;
            transaction
                .execute(self.universe_mut()) // TODO: wrong universe
                .map_err(|e| ToolError::Internal(e.to_string()))?;
            Ok(())
        } else {
            // Otherwise, it's a click inside the game world (even if the cursor hit nothing at all).
            // TODO: if the cursor space is not the game space this should be an error
            if let Some(character_ref) = self.game_character.borrow() {
                let transaction =
                    Character::click(character_ref.clone(), self.cursor_result.as_ref(), button)?;
                transaction
                    .execute(self.universe_mut())
                    .map_err(|e| ToolError::Internal(e.to_string()))?;

                // Spend a little time doing light updates, to ensure that changes right in front of
                // the player are clean (and not flashes of blackness).
                if let Some(space_ref) = self.cursor_result.as_ref().map(|c| &c.space) {
                    // TODO: Instead of ignoring error, log it
                    let _ = space_ref.try_modify(|space| {
                        space.update_lighting_from_queue();
                    });
                }

                Ok(())
            } else {
                Err(ToolError::NoTool)
            }
        }
    }

    /// Returns textual information intended to be overlaid as a HUD on top of the rendered scene
    /// containing diagnostic information about rendering and stepping.
    pub fn info_text<T>(&self, render: T) -> InfoText<'_, T> {
        InfoText { app: self, render }
    }

    #[doc(hidden)] // TODO: Decide whether we want FpsCounter in our public API
    pub fn draw_fps_counter(&self) -> &FpsCounter {
        self.frame_clock.draw_fps_counter()
    }
}

/// A collection of values associated with each of the layers of graphics that
/// is normally drawn (HUD on top of world, currently).
// Exhaustive: Changing this will probably be breaking anyway, until we make it a
// more thorough abstraction.
#[allow(clippy::exhaustive_structs)]
pub struct Layers<T> {
    pub world: T,
    pub ui: T,
}

impl<T> Layers<T> {
    pub(crate) fn try_map_ref<U, E>(
        &self,
        mut f: impl FnMut(&T) -> Result<U, E>,
    ) -> Result<Layers<U>, E> {
        Ok(Layers {
            world: f(&self.world)?,
            ui: f(&self.ui)?,
        })
    }
}

pub struct StandardCameras {
    /// Cameras are synced with this
    graphics_options: ListenableSource<GraphicsOptions>,
    graphics_options_dirty: DirtyFlag,

    character_source: ListenableSource<Option<URef<Character>>>,
    /// Tracks whether the character was replaced (not whether its view changed).
    character_dirty: DirtyFlag,
    character: Option<URef<Character>>,

    ui_space: Option<URef<Space>>,
    viewport_dirty: bool,

    cameras: Layers<Camera>,
}

impl StandardCameras {
    pub fn from_app_state(
        app_state: &AllIsCubesAppState,
        viewport: Viewport,
    ) -> Result<Self, std::convert::Infallible> {
        let graphics_options = app_state.graphics_options();
        let graphics_options_dirty = DirtyFlag::new(false);
        graphics_options.listen(graphics_options_dirty.listener());
        let initial_options = &*graphics_options.get();

        let character_source = app_state.character();
        let character_dirty = DirtyFlag::new(true);
        character_source.listen(character_dirty.listener());

        let mut this = Self {
            graphics_options,
            graphics_options_dirty,

            character_source,
            character_dirty,
            character: None, // update() will fix this up
            ui_space: Some(app_state.ui_space().clone()),

            viewport_dirty: true,

            cameras: Layers {
                ui: Camera::new(Vui::graphics_options(initial_options.clone()), viewport),
                world: Camera::new(initial_options.clone(), viewport),
            },
        };

        this.update();
        Ok(this)
    }

    /// Updates camera state from data sources.
    ///
    /// This should be called at the beginning of each frame or as needed when the
    /// cameras are to be used.
    pub fn update(&mut self) {
        let options_dirty = self.graphics_options_dirty.get_and_clear();
        if options_dirty {
            let current_options = self.graphics_options.snapshot();
            self.cameras.world.set_options(current_options.clone());
            self.cameras
                .ui
                .set_options(Vui::graphics_options(current_options));
        }

        // Update UI view if the FOV changed or the viewport did
        let viewport_dirty = mem::take(&mut self.viewport_dirty);
        if options_dirty || viewport_dirty {
            if let Some(space_ref) = &self.ui_space {
                // TODO: try_borrow()
                // TODO: ...or just skip the whole idea
                self.cameras.ui.set_view_transform(Vui::view_transform(
                    &*space_ref.borrow(),
                    self.cameras.ui.fov_y(),
                ));
            }
        }

        if self.character_dirty.get_and_clear() {
            self.character = self.character_source.snapshot();
            if self.character.is_none() {
                // TODO: set an error flag saying that nothing should be drawn
                self.cameras.world.set_view_transform(One::one());
            }
        }

        if let Some(character_ref) = &self.character {
            #[allow(clippy::single_match)]
            match character_ref.try_borrow() {
                Ok(character) => {
                    self.cameras.world.set_view_transform(character.view());
                }
                Err(_) => {
                    // TODO: set an error flag indicating failure to update
                }
            }
        }
    }

    pub fn graphics_options(&self) -> &GraphicsOptions {
        self.cameras.world.options()
    }

    pub fn cameras(&self) -> &Layers<Camera> {
        &self.cameras
    }

    /// Returns the character's viewpoint to draw in the world layer.
    /// May be [`None`] if there is no current character.
    pub fn character(&self) -> Option<&URef<Character>> {
        self.character.as_ref()
    }

    pub fn ui_space(&self) -> Option<&URef<Space>> {
        self.ui_space.as_ref()
    }

    pub fn viewport(&self) -> Viewport {
        self.cameras.world.viewport()
    }

    pub fn set_viewport(&mut self, viewport: Viewport) {
        // TODO: this should be an iter_mut() or something
        self.cameras.world.set_viewport(viewport);
        self.cameras.ui.set_viewport(viewport);
    }

    /// Perform a raycast through these cameras to find what the cursor hits.
    ///
    /// Make sure to call [`StandardCameras::update`] first so that the cameras are
    /// up to date with game state.
    pub fn project_cursor(&self, ndc_pos: Point2<FreeCoordinate>) -> Option<Cursor> {
        if let Some(ui_space_ref) = self.ui_space.as_ref() {
            let ray = self.cameras.ui.project_ndc_into_world(ndc_pos);
            if let Some(cursor) = cursor_raycast(ray, ui_space_ref, FreeCoordinate::INFINITY) {
                return Some(cursor);
            }
        }

        if let Some(character_ref) = self.character.as_ref() {
            let ray = self.cameras.world.project_ndc_into_world(ndc_pos);
            // TODO: maximum distance should be determined by character/universe parameters instead of hardcoded
            if let Some(cursor) = cursor_raycast(ray, &character_ref.borrow().space, 6.0) {
                return Some(cursor);
            }
        }

        None
    }
}

/// A message sent to the [`AllIsCubesAppState`], such as from a user interface element.
// TODO: make public if this proves to be a good approach
#[derive(Clone, Debug)]
#[non_exhaustive]
pub(crate) enum ControlMessage {
    TogglePause,
    ToggleMouselook,
}

#[derive(Copy, Clone, Debug)]
pub struct InfoText<'a, T> {
    app: &'a AllIsCubesAppState,
    render: T,
}

impl<T: CustomFormat<StatusText>> Display for InfoText<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(character_ref) = self.app.game_character.borrow() {
            write!(f, "{}", character_ref.borrow().custom_format(StatusText)).unwrap();
        }
        write!(
            f,
            "\n\n{:#?}\n\nFPS: {:2.1}\n{:#?}\n\n",
            self.app.last_step_info.custom_format(StatusText),
            self.app.frame_clock.draw_fps_counter().frames_per_second(),
            self.render.custom_format(StatusText),
        )?;
        match self.app.cursor_result() {
            Some(cursor) => write!(f, "{}", cursor),
            None => write!(f, "No block"),
        }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures_channel::oneshot;

    use crate::apps::AllIsCubesAppState;
    use crate::space::Space;
    use crate::universe::{Name, Universe, UniverseIndex};

    #[test]
    fn set_universe_async() {
        let old_marker = Name::from("old");
        let new_marker = Name::from("new");
        let mut app = AllIsCubesAppState::new();
        app.universe_mut()
            .insert(old_marker.clone(), Space::empty_positive(1, 1, 1))
            .unwrap();

        // Set up async loading but don't deliver anything yet
        let (send, recv) = oneshot::channel();
        app.set_universe_async(async { recv.await.unwrap() });

        // Existing universe should still be present.
        app.maybe_step_universe();
        assert!(UniverseIndex::<Space>::get(app.universe_mut(), &old_marker).is_some());

        // Deliver new universe.
        let mut new_universe = Universe::new();
        new_universe
            .insert(new_marker.clone(), Space::empty_positive(1, 1, 1))
            .unwrap();
        send.send(Ok(new_universe)).unwrap();

        // Receive it.
        app.maybe_step_universe();
        assert!(UniverseIndex::<Space>::get(app.universe_mut(), &new_marker).is_some());
        assert!(UniverseIndex::<Space>::get(app.universe_mut(), &old_marker).is_none());

        // Verify cleanup (that the next step can succeed).
        app.maybe_step_universe();
    }
}
