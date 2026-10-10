//! `sf-state` — live project state for StreamFlow Lite.
//!
//! [`State`] is the in-memory model the engine mutates while running: inputs
//! (each with its own [`InputColor`] grade and [`FilterStack`]), scenes (with
//! scene-level filter stacks), and program-level filters. It serializes as
//! JSON so the UI can snapshot and restore it; the on-disk `.streamflow`
//! project format lives in `sf-project`.

use serde::{Deserialize, Serialize};
use sf_color::ColorGrade;
use sf_filters::FilterStack;

/// Per-input color state: which input the grade belongs to, and the grade.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputColor {
    /// Id of the input this grade applies to.
    pub input_id: String,
    /// The input's primary color grade (UI units).
    pub grade: ColorGrade,
}

/// Live state of one input (camera / screen / media source).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputState {
    /// Stable input id (e.g. `"cam1"`).
    pub id: String,
    /// Display name shown in the UI (e.g. `"Camera 1"`).
    pub name: String,
    /// The input's color grade state.
    pub color: InputColor,
    /// Input-level filter stack.
    pub filters: FilterStack,
    /// Whether the input is enabled.
    pub enabled: bool,
}

/// Live state of one scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneState {
    /// Stable scene id.
    pub id: String,
    /// Display name shown in the UI.
    pub name: String,
    /// Scene-level filter stack.
    pub filters: FilterStack,
}

/// The engine's live project state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// All inputs by id.
    pub inputs: Vec<InputState>,
    /// All scenes by id.
    pub scenes: Vec<SceneState>,
    /// Program-level (global output) filter stack.
    pub program_filters: FilterStack,
}

impl State {
    /// Creates an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an input with a default (identity) grade, empty filter stack, and
    /// `enabled = true`.
    pub fn add_input(&mut self, id: &str, name: &str) {
        self.inputs.push(InputState {
            id: id.to_string(),
            name: name.to_string(),
            color: InputColor {
                input_id: id.to_string(),
                grade: ColorGrade::default(),
            },
            filters: FilterStack::new(),
            enabled: true,
        });
    }

    /// Returns the color state of the input with `id`, if it exists.
    pub fn input_color(&self, id: &str) -> Option<&InputColor> {
        self.inputs.iter().find(|i| i.id == id).map(|i| &i.color)
    }

    /// Returns a mutable reference to the color state of the input with `id`.
    pub fn input_color_mut(&mut self, id: &str) -> Option<&mut InputColor> {
        self.inputs
            .iter_mut()
            .find(|i| i.id == id)
            .map(|i| &mut i.color)
    }

    /// "Apply to all cameras": clones `grade` into every input's color state.
    pub fn apply_grade_to_all_cameras(&mut self, grade: &ColorGrade) {
        for input in &mut self.inputs {
            input.color.grade = grade.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_input_defaults() {
        let mut state = State::new();
        state.add_input("cam1", "Camera 1");
        let color = state.input_color("cam1").unwrap();
        assert_eq!(color.input_id, "cam1");
        assert!(color.grade.is_identity());
        assert!(state.input_color("nope").is_none());
    }

    #[test]
    fn apply_grade_to_all_cameras_sets_grade_on_every_input() {
        let mut state = State::new();
        state.add_input("cam1", "Camera 1");
        state.add_input("cam2", "Camera 2");
        state.add_input("cam3", "Camera 3");

        let grade = ColorGrade {
            exposure: 0.5,
            contrast: 10.0,
            temperature: 5600.0,
            ..Default::default()
        };
        state.apply_grade_to_all_cameras(&grade);

        for id in ["cam1", "cam2", "cam3"] {
            let color = state.input_color(id).unwrap();
            assert_eq!(color.grade, grade, "grade mismatch on {id}");
        }
    }

    #[test]
    fn input_color_mut_edits_in_place() {
        let mut state = State::new();
        state.add_input("cam1", "Camera 1");
        state.input_color_mut("cam1").unwrap().grade.exposure = 1.0;
        assert_eq!(state.input_color("cam1").unwrap().grade.exposure, 1.0);
        assert!(state.input_color_mut("nope").is_none());
    }
}
