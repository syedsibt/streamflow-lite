//! `sf-ipc` — the UI ↔ engine command/event contract for StreamFlow Lite.
//!
//! The UI process (Tauri + React) only ever sends parameters; the engine
//! applies everything on the GPU. Commands flow UI → engine, events flow
//! engine → UI, all as JSON with a `"cmd"` / `"event"` discriminator tag:
//!
//! ```json
//! {"cmd":"color.set_param","input_id":"cam1","param":"exposure","value":0.5}
//! {"event":"color.changed","input_id":"cam1"}
//! ```
//!
//! With `#[serde(tag = "cmd")]` the variant fields flatten alongside the tag,
//! so the JSON is a single flat object.

use serde::{Deserialize, Serialize};
use sf_filters::Filter;
use uuid::Uuid;

/// Where a filter-stack command or event applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterTarget {
    /// An input-level filter stack.
    Input(String),
    /// A scene-level filter stack.
    Scene(String),
    /// The program-level (global output) filter stack.
    Program,
}

/// Commands sent from the UI to the engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    /// Set one scalar grade parameter on an input (UI units).
    #[serde(rename = "color.set_param")]
    ColorSetParam {
        /// Target input id.
        input_id: String,
        /// Parameter name, e.g. `"exposure"`, `"temperature"`, `"contrast"`.
        param: String,
        /// New value in UI units.
        value: f32,
    },
    /// Replace one curve's control points on an input.
    #[serde(rename = "color.set_curve")]
    ColorSetCurve {
        /// Target input id.
        input_id: String,
        /// Curve name: `master`, `r`, `g`, `b`, `luma`, `hue_vs_sat`.
        curve: String,
        /// Control points as `(x, y)` pairs in `0..=1`.
        points: Vec<[f32; 2]>,
    },
    /// Load a `.cube` LUT onto an input.
    #[serde(rename = "color.load_lut")]
    ColorLoadLut {
        /// Target input id.
        input_id: String,
        /// Relative path to the `.cube` file.
        path: String,
        /// `"before"` or `"after"` (see [`sf_color::LutPosition`]).
        position: String,
        /// Blend amount `0.0..=1.0`.
        intensity: f32,
    },
    /// Reset grade parameters on an input; `section` optionally limits the
    /// reset (`"basic"`, `"wheels"`, `"curves"`, `"lut"`), `None` resets all.
    #[serde(rename = "color.reset")]
    ColorReset {
        /// Target input id.
        input_id: String,
        /// Optional section to reset; `None` = everything.
        section: Option<String>,
    },
    /// AI camera match: match `target_id` to the `reference_id` look.
    #[serde(rename = "color.match")]
    ColorMatch {
        /// Input id of the reference camera.
        reference_id: String,
        /// Input id of the camera to adjust.
        target_id: String,
    },
    /// AI auto color on an input (lands as editable parameters).
    #[serde(rename = "color.auto")]
    ColorAuto {
        /// Target input id.
        input_id: String,
    },
    /// Add a filter slot to a target's stack.
    #[serde(rename = "filter.add")]
    FilterAdd {
        /// Which stack receives the filter.
        target: FilterTarget,
        /// The filter to add (the engine assigns the slot id).
        filter: Filter,
    },
    /// Drag-reorder a target's stack; `order` must hold exactly the current ids.
    #[serde(rename = "filter.reorder")]
    FilterReorder {
        /// Which stack to reorder.
        target: FilterTarget,
        /// Slot ids in the new order.
        order: Vec<Uuid>,
    },
    /// Set a slot's intensity (clamped `0..=1`).
    #[serde(rename = "filter.set_intensity")]
    FilterSetIntensity {
        /// Which stack owns the slot.
        target: FilterTarget,
        /// Slot id.
        slot_id: Uuid,
        /// New intensity `0.0..=1.0`.
        intensity: f32,
    },
    /// Enable or disable a slot.
    #[serde(rename = "filter.toggle")]
    FilterToggle {
        /// Which stack owns the slot.
        target: FilterTarget,
        /// Slot id.
        slot_id: Uuid,
        /// New enabled state.
        enabled: bool,
    },
    /// Apply a look preset to a target's stack (one click + intensity slider).
    #[serde(rename = "look.apply")]
    LookApply {
        /// Which stack receives the look.
        target: FilterTarget,
        /// Look preset id (`.sflook` file stem).
        look_id: String,
        /// Look strength `0.0..=1.0`.
        intensity: f32,
    },
    /// Subscribe to scope frames for an input (`"waveform"`, `"vectorscope"`,
    /// `"histogram"`, `"false_color"`, `"zebra"`).
    #[serde(rename = "scopes.subscribe")]
    ScopesSubscribe {
        /// Target input id.
        input_id: String,
        /// Scope kind.
        kind: String,
    },
    /// Remove a scope subscription; the engine stops computing that scope
    /// for the input (no GPU scope work runs with zero subscribers).
    #[serde(rename = "scopes.unsubscribe")]
    ScopesUnsubscribe {
        /// Target input id.
        input_id: String,
        /// Scope kind.
        kind: String,
    },
}

/// Events sent from the engine to the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    /// An input's grade changed (echo after a color command).
    #[serde(rename = "color.changed")]
    ColorChanged {
        /// Target input id.
        input_id: String,
    },
    /// A filter stack changed (add/remove/reorder/intensity/toggle/look).
    #[serde(rename = "filter.stack_changed")]
    FilterStackChanged {
        /// Which stack changed.
        target: FilterTarget,
    },
    /// One scope frame payload (15 Hz while subscribed; payload travels
    /// out-of-band or in a follow-up field in later phases).
    #[serde(rename = "scopes.frame")]
    ScopesFrame {
        /// Target input id.
        input_id: String,
        /// Monotonic frame sequence number.
        seq: u64,
    },
    /// Camera-match progress `0.0..=1.0`.
    #[serde(rename = "color.match_progress")]
    ColorMatchProgress {
        /// Target input id.
        input_id: String,
        /// Progress `0.0..=1.0`.
        progress: f32,
    },
    /// Camera-match finished.
    #[serde(rename = "color.match_done")]
    ColorMatchDone {
        /// Target input id.
        input_id: String,
    },
    /// GPU cost warning (e.g. too many heavy filters on a stack).
    #[serde(rename = "gpu.cost_warning")]
    GpuCostWarning {
        /// Human-readable warning for the UI toast.
        message: String,
    },
    /// Drift guard suggestion (C6): lighting drift detected on an input.
    /// The UI should offer a re-match — never auto-apply.
    #[serde(rename = "color.drift_suggest")]
    ColorDriftSuggest {
        /// Target input id.
        input_id: String,
        /// Human-readable suggestion for the UI toast.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_filters::EffectKind;

    #[test]
    fn color_set_param_roundtrip_and_tag() {
        let cmd = Command::ColorSetParam {
            input_id: "cam1".into(),
            param: "exposure".into(),
            value: 0.5,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.contains("\"cmd\":\"color.set_param\""),
            "missing cmd tag: {json}"
        );
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn filter_stack_changed_roundtrip_and_tag() {
        let event = Event::FilterStackChanged {
            target: FilterTarget::Program,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"event\":\"filter.stack_changed\""),
            "missing event tag: {json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn all_commands_have_expected_tags() {
        let slot_id = Uuid::new_v4();
        let cases: Vec<(Command, &str)> = vec![
            (
                Command::ColorSetCurve {
                    input_id: "cam1".into(),
                    curve: "master".into(),
                    points: vec![[0.0, 0.0], [1.0, 1.0]],
                },
                "color.set_curve",
            ),
            (
                Command::ColorLoadLut {
                    input_id: "cam1".into(),
                    path: "color/luts/x.cube".into(),
                    position: "after".into(),
                    intensity: 0.8,
                },
                "color.load_lut",
            ),
            (
                Command::ColorReset {
                    input_id: "cam1".into(),
                    section: None,
                },
                "color.reset",
            ),
            (
                Command::ColorMatch {
                    reference_id: "cam1".into(),
                    target_id: "cam2".into(),
                },
                "color.match",
            ),
            (
                Command::ColorAuto {
                    input_id: "cam1".into(),
                },
                "color.auto",
            ),
            (
                Command::FilterAdd {
                    target: FilterTarget::Input("cam1".into()),
                    filter: Filter::Effect {
                        kind: EffectKind::Vignette,
                        params: EffectKind::Vignette.default_params(),
                    },
                },
                "filter.add",
            ),
            (
                Command::FilterReorder {
                    target: FilterTarget::Scene("main".into()),
                    order: vec![slot_id],
                },
                "filter.reorder",
            ),
            (
                Command::FilterSetIntensity {
                    target: FilterTarget::Program,
                    slot_id,
                    intensity: 0.5,
                },
                "filter.set_intensity",
            ),
            (
                Command::FilterToggle {
                    target: FilterTarget::Program,
                    slot_id,
                    enabled: false,
                },
                "filter.toggle",
            ),
            (
                Command::LookApply {
                    target: FilterTarget::Input("cam1".into()),
                    look_id: "noir".into(),
                    intensity: 0.9,
                },
                "look.apply",
            ),
            (
                Command::ScopesSubscribe {
                    input_id: "cam1".into(),
                    kind: "waveform".into(),
                },
                "scopes.subscribe",
            ),
            (
                Command::ScopesUnsubscribe {
                    input_id: "cam1".into(),
                    kind: "waveform".into(),
                },
                "scopes.unsubscribe",
            ),
        ];
        for (cmd, tag) in cases {
            let json = serde_json::to_string(&cmd).unwrap();
            let expect = format!("\"cmd\":\"{tag}\"");
            assert!(json.contains(&expect), "tag {tag} missing: {json}");
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back);
        }
    }

    #[test]
    fn all_events_have_expected_tags() {
        let cases: Vec<(Event, &str)> = vec![
            (
                Event::ColorChanged {
                    input_id: "cam1".into(),
                },
                "color.changed",
            ),
            (
                Event::ScopesFrame {
                    input_id: "cam1".into(),
                    seq: 42,
                },
                "scopes.frame",
            ),
            (
                Event::ColorMatchProgress {
                    input_id: "cam2".into(),
                    progress: 0.5,
                },
                "color.match_progress",
            ),
            (
                Event::ColorMatchDone {
                    input_id: "cam2".into(),
                },
                "color.match_done",
            ),
            (
                Event::GpuCostWarning {
                    message: "Filter cost high".into(),
                },
                "gpu.cost_warning",
            ),
            (
                Event::ColorDriftSuggest {
                    input_id: "cam2".into(),
                    message: "cam2 exposure drift ho gaya — re-match?".into(),
                },
                "color.drift_suggest",
            ),
        ];
        for (event, tag) in cases {
            let json = serde_json::to_string(&event).unwrap();
            let expect = format!("\"event\":\"{tag}\"");
            assert!(json.contains(&expect), "tag {tag} missing: {json}");
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back);
        }
    }

    #[test]
    fn filter_target_shapes() {
        let json = serde_json::to_string(&FilterTarget::Input("cam1".into())).unwrap();
        assert_eq!(json, "{\"input\":\"cam1\"}");
        let json = serde_json::to_string(&FilterTarget::Program).unwrap();
        assert_eq!(json, "\"program\"");
        let back: FilterTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, FilterTarget::Program);
    }

    #[test]
    fn filter_look_add_payload_shape() {
        // A realistic filter.add payload: flat cmd object with nested filter.
        let cmd = Command::FilterAdd {
            target: FilterTarget::Input("cam1".into()),
            filter: Filter::Look {
                preset_id: "teal-orange".into(),
                intensity: 0.75,
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"filter.add\""));
        assert!(json.contains("\"preset_id\":\"teal-orange\""));
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }
}
