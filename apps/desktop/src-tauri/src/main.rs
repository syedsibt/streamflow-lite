//! StreamFlow Lite desktop shell — Tauri 2 adapter.
//!
//! THIN ADAPTER BY DESIGN: every command below delegates to
//! [`sf_ui_backend::Shell`], which owns all engine-binding logic and is
//! fully unit-tested headless (see `apps/desktop/backend`). This file only
//! maps Tauri commands/events to `Shell` methods.
//!
//! Conventions:
//! - Command argument names are `snake_case` and must match these Rust
//!   signatures exactly (the frontend's IPC client uses the same names).
//! - Engine → UI events are polled from `Shell::drain_events()` on a 66 ms
//!   background thread and re-emitted as Tauri `"sf-event"` payloads; the
//!   frontend routes on the spec §9 `event` tag inside each payload.
//! - The media pipeline (`Shell::av_tick`: render + encode) runs on its own
//!   fixed 33.3 ms background thread (30 fps master tick), separate from the
//!   66 ms UI event poll, so slow UI work can never stall the program feed.
//! - `preview_get_frame` returns a PNG data URL throttled to ~10 Hz by the
//!   backend. Production will replace this with a faster zero-copy path
//!   (shared texture / WebView streaming) — the command shape stays.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::Duration;

use sf_filters::Filter;
use sf_ipc::FilterTarget;
use sf_ui_backend::{
    AiResultView, ChannelStrip, EffectKindInfo, LookInfo, Macro, MacroAction, MacroDispatchResult,
    ProdScene, ProductionStateView, Shell, ShellError, SlotView, SourceState, TransitionView,
    UiEvent,
};
use tauri::{AppHandle, Emitter, Manager, State};

struct AppState(Mutex<Shell>);

type CmdResult<T> = Result<T, String>;

fn lock<'a>(state: &'a State<'a, AppState>) -> CmdResult<std::sync::MutexGuard<'a, Shell>> {
    state
        .0
        .lock()
        .map_err(|e| format!("shell lock poisoned: {e}"))
}

fn ue(e: ShellError) -> String {
    e.to_user_string()
}

// ---------------------------------------------------------------------------
// Color commands (spec §9)
// ---------------------------------------------------------------------------

/// `color.set_param` — the frontend throttles slider drags to 60 Hz (spec §8).
#[tauri::command(rename_all = "snake_case")]
fn color_set_param(
    state: State<'_, AppState>,
    input_id: String,
    param: String,
    value: f32,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?
        .set_param(&input_id, &param, value)
        .map_err(ue)
}

/// Shell extension for spec §8 "double-click a slider = reset".
#[tauri::command(rename_all = "snake_case")]
fn color_reset_param(
    state: State<'_, AppState>,
    input_id: String,
    param: String,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.reset_param(&input_id, &param).map_err(ue)
}

/// `color.set_curve`.
#[tauri::command(rename_all = "snake_case")]
fn color_set_curve(
    state: State<'_, AppState>,
    input_id: String,
    curve: String,
    points: Vec<[f32; 2]>,
) -> CmdResult<()> {
    lock(&state)?
        .set_curve(&input_id, &curve, points)
        .map_err(ue)
}

/// `color.load_lut` — `position`: `"before"` | `"after"`.
#[tauri::command(rename_all = "snake_case")]
fn color_load_lut(
    state: State<'_, AppState>,
    input_id: String,
    path: String,
    position: String,
    intensity: f32,
) -> CmdResult<()> {
    lock(&state)?
        .load_lut(&input_id, &path, &position, intensity)
        .map_err(ue)
}

/// `color.reset` — `section`: `"basic"` | `"wheels"` | `"curves"` | `"lut"` | null.
#[tauri::command(rename_all = "snake_case")]
fn color_reset(
    state: State<'_, AppState>,
    input_id: String,
    section: Option<String>,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?
        .reset(&input_id, section.as_deref())
        .map_err(ue)
}

/// `color.auto` — one-click AI auto color (editable params + explanations).
#[tauri::command(rename_all = "snake_case")]
fn color_auto(state: State<'_, AppState>, input_id: String) -> CmdResult<AiResultView> {
    lock(&state)?.color_auto(&input_id).map_err(ue)
}

/// `color.match` — AI camera match.
#[tauri::command(rename_all = "snake_case")]
fn color_match(
    state: State<'_, AppState>,
    reference_id: String,
    target_id: String,
) -> CmdResult<AiResultView> {
    lock(&state)?
        .color_match(&reference_id, &target_id)
        .map_err(ue)
}

/// Applies a full grade snapshot (A/B slots, before/after compare).
#[tauri::command(rename_all = "snake_case")]
fn apply_grade(
    state: State<'_, AppState>,
    input_id: String,
    grade: sf_color::ColorGrade,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.apply_grade(&input_id, grade).map_err(ue)
}

/// Current grade snapshot (slider positions).
#[tauri::command(rename_all = "snake_case")]
fn grade_snapshot(state: State<'_, AppState>, input_id: String) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.grade_snapshot(&input_id).map_err(ue)
}

// ---------------------------------------------------------------------------
// Filter / look commands (spec §9)
// ---------------------------------------------------------------------------

/// `filter.add`.
#[tauri::command(rename_all = "snake_case")]
fn filter_add(
    state: State<'_, AppState>,
    target: FilterTarget,
    filter: Filter,
) -> CmdResult<String> {
    lock(&state)?
        .filter_add(target, filter)
        .map(|id| id.to_string())
        .map_err(ue)
}

/// `filter.reorder` — `order`: slot ids in the new order.
#[tauri::command(rename_all = "snake_case")]
fn filter_reorder(
    state: State<'_, AppState>,
    target: FilterTarget,
    order: Vec<String>,
) -> CmdResult<()> {
    let order = order
        .iter()
        .map(|s| {
            s.parse::<uuid::Uuid>()
                .map_err(|_| format!("bad slot id: {s}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    lock(&state)?.filter_reorder(target, order).map_err(ue)
}

/// `filter.set_intensity`.
#[tauri::command(rename_all = "snake_case")]
fn filter_set_intensity(
    state: State<'_, AppState>,
    target: FilterTarget,
    slot_id: String,
    intensity: f32,
) -> CmdResult<()> {
    let slot_id = slot_id
        .parse::<uuid::Uuid>()
        .map_err(|_| format!("bad slot id: {slot_id}"))?;
    lock(&state)?
        .filter_set_intensity(target, slot_id, intensity)
        .map_err(ue)
}

/// `filter.toggle` — one-click bypass.
#[tauri::command(rename_all = "snake_case")]
fn filter_toggle(
    state: State<'_, AppState>,
    target: FilterTarget,
    slot_id: String,
    enabled: bool,
) -> CmdResult<()> {
    let slot_id = slot_id
        .parse::<uuid::Uuid>()
        .map_err(|_| format!("bad slot id: {slot_id}"))?;
    lock(&state)?
        .filter_toggle(target, slot_id, enabled)
        .map_err(ue)
}

/// Removes a slot (UI affordance).
#[tauri::command(rename_all = "snake_case")]
fn filter_remove(
    state: State<'_, AppState>,
    target: FilterTarget,
    slot_id: String,
) -> CmdResult<()> {
    let slot_id = slot_id
        .parse::<uuid::Uuid>()
        .map_err(|_| format!("bad slot id: {slot_id}"))?;
    lock(&state)?.filter_remove(target, slot_id).map_err(ue)
}

/// `look.apply` — one click + intensity.
#[tauri::command(rename_all = "snake_case")]
fn look_apply(
    state: State<'_, AppState>,
    target: FilterTarget,
    look_id: String,
    intensity: f32,
) -> CmdResult<String> {
    lock(&state)?
        .look_apply(target, &look_id, intensity)
        .map(|id| id.to_string())
        .map_err(ue)
}

/// Stack snapshot for the Filters tab.
#[tauri::command(rename_all = "snake_case")]
fn stack_snapshot(state: State<'_, AppState>, target: FilterTarget) -> CmdResult<Vec<SlotView>> {
    lock(&state)?.stack_snapshot(&target).map_err(ue)
}

/// Built-in looks for the looks strip.
#[tauri::command(rename_all = "snake_case")]
fn list_looks(state: State<'_, AppState>) -> Vec<LookInfo> {
    lock(&state).map(|mut s| s.list_looks()).unwrap_or_default()
}

/// Effect-kind catalog for the Filters tab "add" menu.
#[tauri::command(rename_all = "snake_case")]
fn list_effect_kinds(state: State<'_, AppState>) -> Vec<EffectKindInfo> {
    lock(&state)
        .map(|s| s.list_effect_kinds())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Scopes + preview
// ---------------------------------------------------------------------------

/// `scopes.subscribe` — `kind`: waveform|vectorscope|histogram|false_color|zebra.
#[tauri::command(rename_all = "snake_case")]
fn scopes_subscribe(state: State<'_, AppState>, input_id: String, kind: String) -> CmdResult<()> {
    lock(&state)?.scopes_subscribe(&input_id, &kind).map_err(ue)
}

/// `scopes.unsubscribe`.
#[tauri::command(rename_all = "snake_case")]
fn scopes_unsubscribe(state: State<'_, AppState>, input_id: String, kind: String) -> CmdResult<()> {
    lock(&state)?
        .scopes_unsubscribe(&input_id, &kind)
        .map_err(ue)
}

/// `preview.get_frame` — PNG data URL, ~10 Hz throttled by the backend.
/// Production will replace this with a zero-copy path; the command shape stays.
#[tauri::command(rename_all = "snake_case")]
fn preview_get_frame(
    state: State<'_, AppState>,
    input_id: String,
    width: u32,
    height: u32,
) -> CmdResult<String> {
    let png = lock(&state)?
        .preview_png(&input_id, width, height)
        .map_err(ue)?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png)
    ))
}

fn png_data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png)
    )
}

// ---------------------------------------------------------------------------
// Production screens: scenes + sources
// ---------------------------------------------------------------------------

/// Creates a production scene.
#[tauri::command(rename_all = "snake_case")]
fn scene_create(state: State<'_, AppState>, name: String) -> CmdResult<ProdScene> {
    Ok(lock(&state)?.scene_create(&name))
}

/// Renames a production scene.
#[tauri::command(rename_all = "snake_case")]
fn scene_rename(
    state: State<'_, AppState>,
    scene_id: String,
    name: String,
) -> CmdResult<ProdScene> {
    lock(&state)?.scene_rename(&scene_id, &name).map_err(ue)
}

/// Deletes a production scene (errors on the last scene).
#[tauri::command(rename_all = "snake_case")]
fn scene_delete(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.scene_delete(&scene_id).map_err(ue)
}

/// Duplicates a production scene (fresh source ids + engine inputs).
#[tauri::command(rename_all = "snake_case")]
fn scene_duplicate(state: State<'_, AppState>, scene_id: String) -> CmdResult<ProdScene> {
    lock(&state)?.scene_duplicate(&scene_id).map_err(ue)
}

/// Lists all production scenes.
#[tauri::command(rename_all = "snake_case")]
fn scene_list(state: State<'_, AppState>) -> Vec<ProdScene> {
    lock(&state).map(|s| s.scene_list()).unwrap_or_default()
}

/// Adds a source to a scene. `kind`: `test_pattern` | `color` | `camera` |
/// `screen` | `media` | `text`. `color_*` apply to the `color` kind;
/// `text`/`path` to the `text`/`media` kinds. Pending kinds report
/// `status: "pending"` and are skipped in the composite.
#[tauri::command(rename_all = "snake_case")]
fn source_add(
    state: State<'_, AppState>,
    scene_id: String,
    kind: String,
    name: String,
    color_r: u8,
    color_g: u8,
    color_b: u8,
    text: Option<String>,
    path: Option<String>,
    device: Option<String>,
    width: u32,
    height: u32,
) -> CmdResult<SourceState> {
    lock(&state)?
        .source_add(
            &scene_id,
            &kind,
            &name,
            [color_r, color_g, color_b],
            text.as_deref(),
            path.as_deref(),
            device.as_deref(),
            width,
            height,
        )
        .map_err(ue)
}

/// Lists available camera devices (Windows DirectShow; empty elsewhere).
#[tauri::command(rename_all = "snake_case")]
fn list_cameras(state: State<'_, AppState>) -> Vec<String> {
    lock(&state).map(|s| s.list_cameras()).unwrap_or_default()
}

/// Negotiated camera format for a source, e.g.
/// `"mjpeg 1920x1080 @30fps (via mjpeg)"` — what the camera actually gave,
/// not just what was requested. `None` for non-camera sources.
#[tauri::command(rename_all = "snake_case")]
fn camera_format(state: State<'_, AppState>, source_id: String) -> Option<String> {
    lock(&state).ok().and_then(|s| s.camera_format(&source_id))
}

/// Lists available audio input devices (Windows DirectShow; empty elsewhere).
#[tauri::command(rename_all = "snake_case")]
fn list_audio_devices(state: State<'_, AppState>) -> Vec<String> {
    lock(&state).map(|s| s.list_audio_devices()).unwrap_or_default()
}

/// Active stream encoder name.
#[tauri::command(rename_all = "snake_case")]
fn stream_encoder_name(state: State<'_, AppState>) -> String {
    lock(&state).map(|s| s.stream_encoder_name()).unwrap_or_default()
}

/// Stage timings of the last media tick in ms:
/// `[render, audio, record, stream]` — which stage eats the 33ms budget.
#[tauri::command(rename_all = "snake_case")]
fn perf_stages(state: State<'_, AppState>) -> Vec<f32> {
    lock(&state)
        .map(|s| s.perf_stages().to_vec())
        .unwrap_or_default()
}

/// Performance stats: (fps, render_ms, cpu_percent_estimate, stream_kbps,
/// skipped_render, dropped_network).
#[tauri::command(rename_all = "snake_case")]
fn perf_stats(state: State<'_, AppState>) -> Vec<f32> {
    lock(&state)
        .map(|mut s| {
            let (fps, render_ms, cpu, kbps, skipped, dropped) = s.perf_stats();
            vec![fps, render_ms, cpu, kbps, skipped, dropped]
        })
        .unwrap_or_default()
}

/// Removes a source from a scene.
#[tauri::command(rename_all = "snake_case")]
fn source_remove(state: State<'_, AppState>, scene_id: String, source_id: String) -> CmdResult<()> {
    lock(&state)?
        .source_remove(&scene_id, &source_id)
        .map_err(ue)
}

/// Drag-reorders a scene's sources: `order` holds source ids bottom to top.
#[tauri::command(rename_all = "snake_case")]
fn source_reorder(
    state: State<'_, AppState>,
    scene_id: String,
    order: Vec<String>,
) -> CmdResult<()> {
    lock(&state)?.source_reorder(&scene_id, order).map_err(ue)
}

/// Shows or hides a source.
#[tauri::command(rename_all = "snake_case")]
fn source_set_visible(
    state: State<'_, AppState>,
    scene_id: String,
    source_id: String,
    visible: bool,
) -> CmdResult<SourceState> {
    lock(&state)?
        .source_set_visible(&scene_id, &source_id, visible)
        .map_err(ue)
}

// ---------------------------------------------------------------------------
// Production screens: preview/program, transitions, monitor frames
// ---------------------------------------------------------------------------

/// Full production snapshot (scenes, preview/program, transition, mixer,
/// stream/record lifecycles, macros).
#[tauri::command(rename_all = "snake_case")]
fn production_state(state: State<'_, AppState>) -> CmdResult<ProductionStateView> {
    Ok(lock(&state)?.production_state())
}

/// Sets the preview (standby) scene. Retargets an in-flight transition.
#[tauri::command(rename_all = "snake_case")]
fn production_set_preview(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.production_set_preview(&scene_id).map_err(ue)
}

/// Sets the program scene directly (cancels any in-flight transition).
#[tauri::command(rename_all = "snake_case")]
fn production_set_program(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.production_set_program(&scene_id).map_err(ue)
}

/// Instant cut: program = preview.
#[tauri::command(rename_all = "snake_case")]
fn transition_cut(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.transition_cut().map_err(ue)
}

/// Starts an auto transition. `kind`: `"mix"` (only kind today).
/// The 66 ms poll loop advances it via `transition_tick()`.
#[tauri::command(rename_all = "snake_case")]
fn transition_auto(
    state: State<'_, AppState>,
    duration_ms: u64,
    kind: String,
) -> CmdResult<TransitionView> {
    lock(&state)?
        .transition_auto(duration_ms, &kind)
        .map_err(ue)
}

/// Scene monitor frame as a PNG data URL (graded sources, CPU-composited).
#[tauri::command(rename_all = "snake_case")]
fn scene_preview_png(
    state: State<'_, AppState>,
    scene_id: String,
    width: u32,
    height: u32,
) -> CmdResult<String> {
    let png = lock(&state)?
        .scene_render_png(&scene_id, width, height)
        .map_err(ue)?;
    Ok(png_data_url(&png))
}

// ---------------------------------------------------------------------------
// Production screens: mixer
// ---------------------------------------------------------------------------

/// Sets a source channel gain (`0.0..=2.0`, clamped).
#[tauri::command(rename_all = "snake_case")]
fn mixer_set_gain(
    state: State<'_, AppState>,
    source_id: String,
    gain: f32,
) -> CmdResult<ChannelStrip> {
    lock(&state)?.mixer_set_gain(&source_id, gain).map_err(ue)
}

/// Mutes/unmutes a source channel.
#[tauri::command(rename_all = "snake_case")]
fn mixer_set_mute(
    state: State<'_, AppState>,
    source_id: String,
    mute: bool,
) -> CmdResult<ChannelStrip> {
    lock(&state)?.mixer_set_mute(&source_id, mute).map_err(ue)
}

/// Sets the master gain (`0.0..=2.0`, clamped).
#[tauri::command(rename_all = "snake_case")]
fn mixer_set_master_gain(state: State<'_, AppState>, gain: f32) -> CmdResult<ChannelStrip> {
    Ok(lock(&state)?.mixer_set_master_gain(gain))
}

/// Mutes/unmutes the master.
#[tauri::command(rename_all = "snake_case")]
fn mixer_set_master_mute(state: State<'_, AppState>, mute: bool) -> CmdResult<ChannelStrip> {
    Ok(lock(&state)?.mixer_set_master_mute(mute))
}

// ---------------------------------------------------------------------------
// Production screens: streaming + recording (honest stubs)
// ---------------------------------------------------------------------------

/// Stores stream settings. `platform`: `youtube` | `twitch` | `facebook` |
/// `custom_rtmp`. The key is kept in memory only — never logged, never
/// serialized, never in error strings.
#[tauri::command(rename_all = "snake_case")]
fn stream_set_settings(
    state: State<'_, AppState>,
    url: String,
    key: String,
    platform: String,
) -> CmdResult<()> {
    lock(&state)?
        .stream_set_settings(&url, &key, &platform)
        .map_err(ue)
}

/// Starts streaming. Fails honestly: `sf-output` is a stub, so the
/// lifecycle moves to `error` with a clear message.
#[tauri::command(rename_all = "snake_case")]
fn stream_start(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.stream_start().map_err(ue)
}

/// Stops streaming (any state → idle).
#[tauri::command(rename_all = "snake_case")]
fn stream_stop(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.stream_stop().map_err(ue)
}

/// Stores record settings. `format`: `mp4` | `mkv`.
#[tauri::command(rename_all = "snake_case")]
fn record_set_settings(
    state: State<'_, AppState>,
    path: String,
    format: String,
    quality: String,
) -> CmdResult<()> {
    lock(&state)?
        .record_set_settings(&path, &format, &quality)
        .map_err(ue)
}

/// Starts recording. Fails honestly: `sf-encode` is a stub.
#[tauri::command(rename_all = "snake_case")]
fn record_start(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_start().map_err(ue)
}

/// Stops recording (any state → idle).
#[tauri::command(rename_all = "snake_case")]
fn record_stop(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_stop().map_err(ue)
}

/// Pauses an active recording (elapsed time excludes pauses).
#[tauri::command(rename_all = "snake_case")]
fn record_pause(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_pause().map_err(ue)
}

/// Resumes a paused recording.
#[tauri::command(rename_all = "snake_case")]
fn record_resume(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_resume().map_err(ue)
}

// ---------------------------------------------------------------------------
// Production screens: macros
// ---------------------------------------------------------------------------

/// Binds a macro. `action` is a tagged JSON object, e.g.
/// `{"action":"cut"}`, `{"action":"auto_transition","duration_ms":500}`,
/// `{"action":"toggle_stream"}`, `{"action":"toggle_record"}`,
/// `{"action":"apply_look","look_id":"podcast-warm","intensity":0.8}`.
#[tauri::command(rename_all = "snake_case")]
fn macro_add(
    state: State<'_, AppState>,
    name: String,
    hotkey: String,
    action: MacroAction,
) -> CmdResult<Macro> {
    lock(&state)?.macro_add(&name, &hotkey, action).map_err(ue)
}

/// Removes a macro by id.
#[tauri::command(rename_all = "snake_case")]
fn macro_remove(state: State<'_, AppState>, macro_id: String) -> CmdResult<()> {
    lock(&state)?.macro_remove(&macro_id).map_err(ue)
}

/// Lists all bound macros.
#[tauri::command(rename_all = "snake_case")]
fn macro_list(state: State<'_, AppState>) -> Vec<Macro> {
    lock(&state).map(|s| s.macro_list()).unwrap_or_default()
}

/// Executes the macro bound to `hotkey`; reports what happened.
#[tauri::command(rename_all = "snake_case")]
fn macro_dispatch(state: State<'_, AppState>, hotkey: String) -> CmdResult<MacroDispatchResult> {
    lock(&state)?.macro_dispatch(&hotkey).map_err(ue)
}

/// Forwards one UI event to the frontend under the single `"sf-event"` channel.
fn forward(app: &AppHandle, event: &UiEvent) {
    let _ = app.emit("sf-event", event);
}

fn main() {
    tauri::Builder::default()
        .manage(AppState(Mutex::new(
            Shell::new().expect("engine must open a GPU adapter (headless: lavapipe)"),
        )))
        .setup(|app| {
            // Media pipeline: fixed 33.3 ms master tick (30 fps). Renders one
            // program frame and advances record/stream encoders. Runs on its
            // own thread so UI polling, PNG previews, and transitions can
            // never stall the program feed. If the camera is late, the last
            // frame is repeated (tick never waits).
            let media_handle = app.handle().clone();
            std::thread::spawn(move || loop {
                let tick = std::time::Instant::now();
                if let Ok(mut s) = media_handle.state::<AppState>().0.lock() {
                    // Never panics; backend failures degrade to Error states.
                    s.av_tick();
                }
                // Monotonic fixed cadence: sleep the remainder of 33.3 ms.
                let elapsed = tick.elapsed();
                if elapsed < Duration::from_micros(33_333) {
                    std::thread::sleep(Duration::from_micros(33_333) - elapsed);
                }
            });
            // Engine → UI event pump: advance auto-transitions, then poll
            // the shell and re-emit events. 66 ms is plenty for UI state;
            // it is deliberately decoupled from the 33 ms media tick.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(66));
                let events = handle
                    .state::<AppState>()
                    .0
                    .lock()
                    .map(|mut s| {
                        // Wall-clock transition progress; emits
                        // `production.transition_progress` / `transition_done`.
                        // AV work lives on the media thread above — this loop
                        // only owns UI-facing state.
                        s.transition_tick();
                        s.drain_events()
                    })
                    .unwrap_or_default();
                for e in &events {
                    forward(&handle, e);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            color_set_param,
            color_reset_param,
            apply_grade,
            color_set_curve,
            color_load_lut,
            color_reset,
            color_auto,
            color_match,
            grade_snapshot,
            filter_add,
            filter_reorder,
            filter_set_intensity,
            filter_toggle,
            filter_remove,
            look_apply,
            stack_snapshot,
            list_looks,
            list_effect_kinds,
            scopes_subscribe,
            scopes_unsubscribe,
            preview_get_frame,
            scene_create,
            scene_rename,
            scene_delete,
            scene_duplicate,
            scene_list,
            source_add,
            list_cameras,
            camera_format,
            list_audio_devices,
            perf_stats,
            perf_stages,
            stream_encoder_name,
            source_remove,
            source_reorder,
            source_set_visible,
            production_state,
            production_set_preview,
            production_set_program,
            transition_cut,
            transition_auto,
            scene_preview_png,
            mixer_set_gain,
            mixer_set_mute,
            mixer_set_master_gain,
            mixer_set_master_mute,
            stream_set_settings,
            stream_start,
            stream_stop,
            record_set_settings,
            record_start,
            record_stop,
            record_pause,
            record_resume,
            macro_add,
            macro_remove,
            macro_list,
            macro_dispatch,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
