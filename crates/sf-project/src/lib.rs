//! `sf-project` — the `.streamflow` project format.
//!
//! A project is a directory containing `project.json` plus asset folders:
//!
//! ```text
//! my-show.streamflow/
//! ├── project.json        # [`Project`]: inputs, scenes, program filters
//! └── color/
//!     ├── luts/*.cube     # 3D LUTs, referenced by relative path
//!     ├── looks/*.sflook  # look presets
//!     └── stacks/*.sfstack
//! ```
//!
//! This crate handles the JSON side: [`Project::save_json`] and
//! [`Project::load_json`]. LUT/look paths stay as relative strings inside
//! [`sf_color::LutRef`] / [`sf_filters::LookPreset`]; no path resolution is
//! done here (C0–C3).

use std::path::Path;

use serde::{Deserialize, Serialize};
use sf_color::ColorGrade;
use sf_filters::FilterStack;

/// One input in a saved project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectInput {
    /// Stable input id (e.g. `"cam1"`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Saved color grade (UI units).
    #[serde(default)]
    pub color: ColorGrade,
    /// Saved input-level filter stack.
    #[serde(default)]
    pub filters: FilterStack,
}

/// One scene in a saved project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneDef {
    /// Stable scene id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Saved scene-level filter stack.
    #[serde(default)]
    pub filters: FilterStack,
}

/// Program-level (global output) definition in a saved project.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgramDef {
    /// Saved program-level filter stack.
    #[serde(default)]
    pub filters: FilterStack,
}

/// A saved StreamFlow Lite project (`project.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Project format version (currently `1`).
    pub version: u32,
    /// Project display name.
    pub name: String,
    /// Inputs with their color grades and filter stacks.
    #[serde(default)]
    pub inputs: Vec<ProjectInput>,
    /// Scenes with their filter stacks.
    #[serde(default)]
    pub scenes: Vec<SceneDef>,
    /// Program-level filters.
    #[serde(default)]
    pub program: ProgramDef,
}

impl Project {
    /// Serializes the project as pretty JSON to `path`.
    pub fn save_json(&self, path: &Path) -> Result<(), ProjectError> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Loads a project from a JSON file written by [`Project::save_json`].
    pub fn load_json(path: &Path) -> Result<Self, ProjectError> {
        let text = std::fs::read_to_string(path)?;
        let project = serde_json::from_str(&text)?;
        Ok(project)
    }
}

/// Errors from [`Project::save_json`] / [`Project::load_json`].
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// File read/write failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_color::{LutPosition, LutRef};
    use sf_filters::{Filter, FilterSlot};
    use uuid::Uuid;

    fn look_slot(preset_id: &str) -> FilterSlot {
        FilterSlot {
            id: Uuid::new_v4(),
            filter: Filter::Look {
                preset_id: preset_id.to_string(),
                intensity: 0.8,
            },
            enabled: true,
            intensity: 1.0,
            mask: None,
        }
    }

    fn sample_project() -> Project {
        let mut stack = FilterStack::new();
        stack.push(look_slot("teal-orange")).unwrap();

        Project {
            version: 1,
            name: "Test Show".to_string(),
            inputs: vec![
                ProjectInput {
                    id: "cam1".to_string(),
                    name: "Camera 1".to_string(),
                    color: ColorGrade {
                        exposure: 0.3,
                        contrast: 12.0,
                        temperature: 5600.0,
                        lut: Some(LutRef {
                            file: "color/luts/podcast_warm.cube".to_string(),
                            intensity: 0.6,
                            position: LutPosition::After,
                        }),
                        ..Default::default()
                    },
                    filters: stack,
                },
                ProjectInput {
                    id: "cam2".to_string(),
                    name: "Camera 2".to_string(),
                    color: ColorGrade::default(),
                    filters: FilterStack::new(),
                },
            ],
            scenes: vec![SceneDef {
                id: "main".to_string(),
                name: "Main".to_string(),
                filters: FilterStack::new(),
            }],
            program: ProgramDef::default(),
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let project = sample_project();
        let dir = std::env::temp_dir().join("sf-project-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("project.json");

        project.save_json(&path).unwrap();
        let loaded = Project::load_json(&path).unwrap();

        // Full structural equality.
        assert_eq!(loaded, project);

        // Key-field spot checks.
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.name, "Test Show");
        assert_eq!(loaded.inputs.len(), 2);
        let cam1 = &loaded.inputs[0];
        assert_eq!(cam1.id, "cam1");
        assert_eq!(cam1.color.exposure, 0.3);
        assert_eq!(cam1.color.contrast, 12.0);
        let lut = cam1.color.lut.as_ref().unwrap();
        assert_eq!(lut.file, "color/luts/podcast_warm.cube");
        assert_eq!(lut.position, LutPosition::After);
        assert_eq!(cam1.filters.slots().len(), 1);
        assert!(matches!(
            &cam1.filters.slots()[0].filter,
            Filter::Look { preset_id, .. } if preset_id == "teal-orange"
        ));
        assert_eq!(loaded.inputs[1].color, ColorGrade::default());
        assert_eq!(loaded.scenes.len(), 1);
        assert_eq!(loaded.scenes[0].id, "main");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let err = Project::load_json(Path::new("/nonexistent-dir-xyz/project.json")).unwrap_err();
        assert!(matches!(err, ProjectError::Io(_)));
    }

    #[test]
    fn load_bad_json_is_json_error() {
        let dir = std::env::temp_dir().join("sf-project-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = Project::load_json(&path).unwrap_err();
        assert!(matches!(err, ProjectError::Json(_)));
        std::fs::remove_file(&path).ok();
    }
}
