// Copyright 2020 Kevin Reid under the terms of the MIT License as detailed
// in the accompanying file README.md or <http://opensource.org/licenses/MIT>.

use crate::camera::Camera;
use crate::demo_content::new_universe_with_stuff;
use crate::space::SpaceStepInfo;
use crate::universe::{FrameClock, URef, Universe};

/// Everything that a game application needs regardless of platform.
///
/// Once we have multiplayer / client-server support, this will become the client-side
/// structure.
#[derive(Debug)]
pub struct AllIsCubesAppState {
    universe: Universe,
    camera: URef<Camera>,
    pub frame_clock: FrameClock,
}

impl AllIsCubesAppState {
    /// Construct a new `AllIsCubesAppState` using the result of
    /// `new_universe_with_stuff()` as initial content.
    pub fn new() -> Self {
        let universe = new_universe_with_stuff();
        Self {
            camera: universe.get_default_camera(),
            frame_clock: FrameClock::new(),
            universe,
        }
    }

    /// Returns a reference to the camera that should be shown to the user.
    pub fn camera(&self) -> &URef<Camera> {
        &self.camera
    }

    /// Returns a mutable reference to the universe.
    pub fn universe_mut(&mut self) -> &mut Universe {
        &mut self.universe
    }

    // TODO: Universe should have a proper info struct return
    /// Steps the universe if the `FrameClock` says it's time to do so.
    pub fn maybe_step_universe(&mut self) -> Option<(SpaceStepInfo, ())> {
        if self.frame_clock.should_step() {
            let result = self.universe.step(self.frame_clock.step_length());
            self.frame_clock.did_step();
            Some(result)
        } else {
            None
        }
    }
}

impl Default for AllIsCubesAppState {
    fn default() -> Self {
        Self::new()
    }
}
