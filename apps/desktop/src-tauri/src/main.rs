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
//! - The media pipeline runs on its own thread with an absolute-deadline
//!   clock and passes each tick's *scheduled* time to `Shell::av_tick_at`,
//!   so stream timestamps follow the wall clock even when a tick runs late.
//! - Every command is `async`: it runs off the WebView/main thread, so a
//!   busy engine can never freeze the UI. Monitor frames are taken under the
//!   lock in O(1) and JPEG-encoded after releasing it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::Duration;

use sf_filters::Filter;
use sf_ipc::FilterTarget;
use sf_ui_backend::{
    AiResultView, CameraSourceInfo, ChannelStrip, EffectKindInfo, EnhancerView, LookInfo, Macro,
    MacroAction, MacroDispatchResult, ProdScene, ProductionStateView, Shell, ShellError, SlotView,
    SourceState, SourceTransform, TransitionView, UiEvent, VideoSettings, DiagnosticsView, PlatformInfo, AvSyncView,
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn color_reset_param(
    state: State<'_, AppState>,
    input_id: String,
    param: String,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.reset_param(&input_id, &param).map_err(ue)
}

/// `color.set_curve`.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn color_auto(state: State<'_, AppState>, input_id: String) -> CmdResult<AiResultView> {
    lock(&state)?.color_auto(&input_id).map_err(ue)
}

/// `color.match` — AI camera match.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn apply_grade(
    state: State<'_, AppState>,
    input_id: String,
    grade: sf_color::ColorGrade,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.apply_grade(&input_id, grade).map_err(ue)
}

/// Current grade snapshot (slider positions).
#[tauri::command(async, rename_all = "snake_case")]
fn grade_snapshot(state: State<'_, AppState>, input_id: String) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.grade_snapshot(&input_id).map_err(ue)
}

// ---------------------------------------------------------------------------
// Filter / look commands (spec §9)
// ---------------------------------------------------------------------------

/// `filter.add`.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn stack_snapshot(state: State<'_, AppState>, target: FilterTarget) -> CmdResult<Vec<SlotView>> {
    lock(&state)?.stack_snapshot(&target).map_err(ue)
}

/// Built-in looks for the looks strip.
#[tauri::command(async, rename_all = "snake_case")]
fn list_looks(state: State<'_, AppState>) -> CmdResult<Vec<LookInfo>> {
    lock(&state).map(|mut s| s.list_looks())
}

/// Effect-kind catalog for the Filters tab "add" menu.
#[tauri::command(async, rename_all = "snake_case")]
fn list_effect_kinds(state: State<'_, AppState>) -> CmdResult<Vec<EffectKindInfo>> {
    lock(&state)
        .map(|s| s.list_effect_kinds())
}

// ---------------------------------------------------------------------------
// Scopes + preview
// ---------------------------------------------------------------------------

/// `scopes.subscribe` — `kind`: waveform|vectorscope|histogram|false_color|zebra.
#[tauri::command(async, rename_all = "snake_case")]
fn scopes_subscribe(state: State<'_, AppState>, input_id: String, kind: String) -> CmdResult<()> {
    lock(&state)?.scopes_subscribe(&input_id, &kind).map_err(ue)
}

/// `scopes.unsubscribe`.
#[tauri::command(async, rename_all = "snake_case")]
fn scopes_unsubscribe(state: State<'_, AppState>, input_id: String, kind: String) -> CmdResult<()> {
    lock(&state)?
        .scopes_unsubscribe(&input_id, &kind)
        .map_err(ue)
}

/// `preview.get_frame` — JPEG data URL of the Color Studio preview. When the
/// input is on air this is the exact graded frame the media tick sent to the
/// program/encoder (live tap); `mode` = "after" (default) | "before" | "split".
#[tauri::command(async, rename_all = "snake_case")]
fn preview_get_frame(
    state: State<'_, AppState>,
    input_id: String,
    width: u32,
    height: u32,
    mode: Option<String>,
) -> CmdResult<String> {
    let mode = sf_ui_backend::PreviewMode::parse(mode.as_deref());
    // Under the lock: grab/downscale only. JPEG encode happens after the
    // lock is released so the media tick is never stalled by the preview.
    let frame = lock(&state)?
        .preview_frame(&input_id, width, height, mode)
        .map_err(ue)?;
    let jpg = sf_ui_backend::encode_preview_jpeg(&frame).map_err(ue)?;
    Ok(jpg_data_url(&jpg))
}

/// Removes the active look from a stack.
#[tauri::command(async, rename_all = "snake_case")]
fn look_clear(state: State<'_, AppState>, target: FilterTarget) -> CmdResult<()> {
    lock(&state)?.look_clear(target).map_err(ue)
}

/// Live effect parameters (Glow threshold/radius/softness, skin smoothing).
#[tauri::command(async, rename_all = "snake_case")]
fn filter_set_params(
    state: State<'_, AppState>,
    target: FilterTarget,
    slot_id: String,
    params: sf_filters::EffectParams,
) -> CmdResult<()> {
    let slot_id = slot_id
        .parse::<uuid::Uuid>()
        .map_err(|_| format!("bad slot id: {slot_id}"))?;
    lock(&state)?
        .filter_set_params(target, slot_id, params)
        .map_err(ue)
}

/// Imports a `.cube` into the LUT library (validated; clear error otherwise).
#[tauri::command(async, rename_all = "snake_case")]
fn lut_import(state: State<'_, AppState>, path: String) -> CmdResult<sf_ui_backend::LutInfo> {
    lock(&state)?.lut_import(&path).map_err(ue)
}

#[tauri::command(async, rename_all = "snake_case")]
fn lut_library(state: State<'_, AppState>) -> CmdResult<Vec<sf_ui_backend::LutInfo>> {
    Ok(lock(&state)?.lut_library())
}

#[tauri::command(async, rename_all = "snake_case")]
fn lut_remove(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    lock(&state)?.lut_remove(&id).map_err(ue)
}

#[tauri::command(async, rename_all = "snake_case")]
fn color_preset_save(
    state: State<'_, AppState>,
    input_id: String,
    name: String,
) -> CmdResult<sf_ui_backend::ColorPresetInfo> {
    lock(&state)?.color_preset_save(&input_id, &name).map_err(ue)
}

#[tauri::command(async, rename_all = "snake_case")]
fn color_preset_list(state: State<'_, AppState>) -> CmdResult<Vec<sf_ui_backend::ColorPresetInfo>> {
    Ok(lock(&state)?.color_preset_list())
}

#[tauri::command(async, rename_all = "snake_case")]
fn color_preset_apply(
    state: State<'_, AppState>,
    input_id: String,
    id: String,
) -> CmdResult<sf_color::ColorGrade> {
    lock(&state)?.color_preset_apply(&input_id, &id).map_err(ue)
}

#[tauri::command(async, rename_all = "snake_case")]
fn color_preset_delete(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    lock(&state)?.color_preset_delete(&id).map_err(ue)
}

fn jpg_data_url(jpg: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpg)
    )
}

// ---------------------------------------------------------------------------
// Production screens: scenes + sources
// ---------------------------------------------------------------------------

/// Creates a production scene.
#[tauri::command(async, rename_all = "snake_case")]
fn scene_create(state: State<'_, AppState>, name: String) -> CmdResult<ProdScene> {
    Ok(lock(&state)?.scene_create(&name))
}

/// Renames a production scene.
#[tauri::command(async, rename_all = "snake_case")]
fn scene_rename(
    state: State<'_, AppState>,
    scene_id: String,
    name: String,
) -> CmdResult<ProdScene> {
    lock(&state)?.scene_rename(&scene_id, &name).map_err(ue)
}

/// Deletes a production scene (errors on the last scene).
#[tauri::command(async, rename_all = "snake_case")]
fn scene_delete(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.scene_delete(&scene_id).map_err(ue)
}

/// Duplicates a production scene (fresh source ids + engine inputs).
#[tauri::command(async, rename_all = "snake_case")]
fn scene_duplicate(state: State<'_, AppState>, scene_id: String) -> CmdResult<ProdScene> {
    lock(&state)?.scene_duplicate(&scene_id).map_err(ue)
}

/// Lists all production scenes.
#[tauri::command(async, rename_all = "snake_case")]
fn scene_list(state: State<'_, AppState>) -> CmdResult<Vec<ProdScene>> {
    lock(&state).map(|s| s.scene_list())
}

/// Adds a source to a scene. `kind`: `test_pattern` | `color` | `camera` |
/// `screen` | `media` | `text`. `color_*` apply to the `color` kind;
/// `text`/`path` to the `text`/`media` kinds. Pending kinds report
/// `status: "pending"` and are skipped in the composite.
#[tauri::command(async, rename_all = "snake_case")]
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
    audio_device: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
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
            audio_device.as_deref(),
            width,
            height,
            fps,
        )
        .map_err(ue)
}

/// Lists available camera devices (Windows DirectShow; empty elsewhere).
#[tauri::command(async, rename_all = "snake_case")]
fn list_cameras(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    lock(&state).map(|s| s.list_cameras())
}

/// Negotiated camera format for a source, e.g.
/// `"mjpeg 1920x1080 @30fps (via mjpeg)"` — what the camera actually gave,
/// not just what was requested. `None` for non-camera sources.
#[tauri::command(async, rename_all = "snake_case")]
fn camera_format(state: State<'_, AppState>, source_id: String) -> CmdResult<Option<String>> {
    Ok(lock(&state)?.camera_format(&source_id))
}

/// Audio device label for a source (mic name for audio inputs / camera
/// audio). None for sources without an audio device.
#[tauri::command(async, rename_all = "snake_case")]
fn source_audio_device(state: State<'_, AppState>, source_id: String) -> CmdResult<Option<String>> {
    Ok(lock(&state)?.source_audio_device(&source_id))
}

/// Current video input/output settings for the Video menu panel.
#[tauri::command(async, rename_all = "snake_case")]
fn video_settings_get(state: State<'_, AppState>) -> CmdResult<VideoSettings> {
    lock(&state).map(|s| s.video_settings_get())
}

/// Validates and stores video settings. Output size/fps/encoder/bitrate
/// apply the next time streaming/recording starts; the media-tick cadence
/// follows `output_fps` immediately.
#[tauri::command(async, rename_all = "snake_case")]
fn video_settings_set(state: State<'_, AppState>, settings: VideoSettings) -> CmdResult<()> {
    lock(&state)?.video_settings_set(settings).map_err(ue)
}

/// Pauses/resumes a media source's playback; returns the new paused state.
#[tauri::command(async, rename_all = "snake_case")]
fn media_set_paused(
    state: State<'_, AppState>,
    source_id: String,
    paused: bool,
) -> CmdResult<bool> {
    lock(&state)?
        .media_set_paused(&source_id, paused)
        .map_err(ue)
}

/// Reconfigures a camera source live (device/resolution/fps) and returns
/// what the camera actually negotiated.
#[tauri::command(async, rename_all = "snake_case")]
fn camera_reconfigure(
    state: State<'_, AppState>,
    source_id: String,
    device: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    audio_device: Option<String>,
) -> CmdResult<String> {
    lock(&state)?
        .camera_reconfigure(&source_id, device, width, height, fps, audio_device)
        .map_err(ue)
}

/// All camera sources with live config + negotiated format (Video panel).
#[tauri::command(async, rename_all = "snake_case")]
fn camera_sources(state: State<'_, AppState>) -> CmdResult<Vec<CameraSourceInfo>> {
    lock(&state).map(|s| s.camera_sources())
}

/// Lists available audio input devices (Windows DirectShow; empty elsewhere).
#[tauri::command(async, rename_all = "snake_case")]
fn list_audio_devices(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    lock(&state).map(|s| s.list_audio_devices())
}

/// Active stream encoder name.
#[tauri::command(async, rename_all = "snake_case")]
fn stream_encoder_name(state: State<'_, AppState>) -> CmdResult<String> {
    lock(&state).map(|s| s.stream_encoder_name())
}

/// Stage timings of the last media tick in ms:
/// `[render, audio, record, stream]` — which stage eats the 33ms budget.
#[tauri::command(async, rename_all = "snake_case")]
fn perf_stages(state: State<'_, AppState>) -> CmdResult<Vec<f32>> {
    lock(&state)
        .map(|s| s.perf_stages().to_vec())
}

/// Performance stats: (fps, render_ms, cpu_percent_estimate, stream_kbps,
/// skipped_render, dropped_network, lagged_ticks).
#[tauri::command(async, rename_all = "snake_case")]
fn perf_stats(state: State<'_, AppState>) -> CmdResult<Vec<f32>> {
    lock(&state)
        .map(|mut s| {
            let (fps, render_ms, cpu, kbps, skipped, dropped, lagged) = s.perf_stats();
            vec![fps, render_ms, cpu, kbps, skipped, dropped, lagged]
        })
}

/// Removes a source from a scene.
#[tauri::command(async, rename_all = "snake_case")]
fn source_remove(state: State<'_, AppState>, scene_id: String, source_id: String) -> CmdResult<()> {
    lock(&state)?
        .source_remove(&scene_id, &source_id)
        .map_err(ue)
}

/// Drag-reorders a scene's sources: `order` holds source ids bottom to top.
#[tauri::command(async, rename_all = "snake_case")]
fn source_reorder(
    state: State<'_, AppState>,
    scene_id: String,
    order: Vec<String>,
) -> CmdResult<()> {
    lock(&state)?.source_reorder(&scene_id, order).map_err(ue)
}

/// Shows or hides a source.
#[tauri::command(async, rename_all = "snake_case")]
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

/// Places a source in the canvas (position / size / crop / fit /
/// opacity). `null` = full frame.
#[tauri::command(async, rename_all = "snake_case")]
fn source_set_transform(
    state: State<'_, AppState>,
    scene_id: String,
    source_id: String,
    transform: Option<SourceTransform>,
) -> CmdResult<SourceState> {
    lock(&state)?
        .source_set_transform(&scene_id, &source_id, transform)
        .map_err(ue)
}

/// Arranges a scene's video sources in a preset layout (side by side,
/// picture-in-picture, split, grid, full).
#[tauri::command(async, rename_all = "snake_case")]
fn scene_apply_layout(state: State<'_, AppState>, scene_id: String, layout: String) -> CmdResult<usize> {
    lock(&state)?.scene_apply_layout(&scene_id, &layout).map_err(ue)
}

// ---------------------------------------------------------------------------
// Production screens: preview/program, transitions, monitor frames
// ---------------------------------------------------------------------------

/// Full production snapshot (scenes, preview/program, transition, mixer,
/// stream/record lifecycles, macros).
#[tauri::command(async, rename_all = "snake_case")]
fn production_state(state: State<'_, AppState>) -> CmdResult<ProductionStateView> {
    Ok(lock(&state)?.production_state())
}

/// Sets the preview (standby) scene. Retargets an in-flight transition.
#[tauri::command(async, rename_all = "snake_case")]
fn production_set_preview(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.production_set_preview(&scene_id).map_err(ue)
}

/// Sets the program scene directly (cancels any in-flight transition).
#[tauri::command(async, rename_all = "snake_case")]
fn production_set_program(state: State<'_, AppState>, scene_id: String) -> CmdResult<()> {
    lock(&state)?.production_set_program(&scene_id).map_err(ue)
}

/// Instant cut: program = preview.
#[tauri::command(async, rename_all = "snake_case")]
fn transition_cut(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.transition_cut().map_err(ue)
}

/// Starts an auto transition. `kind`: `"mix"` (only kind today).
/// The 66 ms poll loop advances it via `transition_tick()`.
#[tauri::command(async, rename_all = "snake_case")]
fn transition_auto(
    state: State<'_, AppState>,
    duration_ms: u64,
    kind: String,
) -> CmdResult<TransitionView> {
    lock(&state)?
        .transition_auto(duration_ms, &kind)
        .map_err(ue)
}

/// Scene monitor frame as a JPEG data URL. The program scene is served from
/// the media tick's live monitor frame (10fps, no re-render); other scenes
/// render on demand at monitor size.
#[tauri::command(async, rename_all = "snake_case")]
fn scene_preview_jpg(state: State<'_, AppState>, scene_id: String) -> CmdResult<String> {
    // O(1) under the lock (shared frame); JPEG encode after releasing it,
    // so a monitor poll never holds up the media tick.
    let frame = lock(&state)?.monitor_snapshot(&scene_id).map_err(ue)?;
    let jpg = frame.to_jpeg().map_err(ue)?;
    Ok(jpg_data_url(&jpg))
}

/// Monitor frame only if it changed since `after_seq` (`None` otherwise):
/// the UI polls this at display rate without re-transferring identical
/// frames. Returns `(seq, data_url)`.
#[tauri::command(async, rename_all = "snake_case")]
fn monitor_frame_if_new(
    state: State<'_, AppState>,
    scene_id: String,
    after_seq: u64,
) -> CmdResult<Option<(u64, String)>> {
    let frame = lock(&state)?.monitor_snapshot(&scene_id).map_err(ue)?;
    if frame.seq <= after_seq {
        return Ok(None);
    }
    let jpg = frame.to_jpeg().map_err(ue)?;
    Ok(Some((frame.seq, jpg_data_url(&jpg))))
}

/// Live diagnostics: per-stage timings, fps, drops, buffers, latency
/// split (app vs platform), encoder, stream, CPU/RAM.
#[tauri::command(async, rename_all = "snake_case")]
fn diagnostics(state: State<'_, AppState>) -> CmdResult<DiagnosticsView> {
    Ok(lock(&state)?.diagnostics())
}

/// Streaming platforms (YouTube, Facebook, TikTok, custom RTMP) with
/// default servers and help text.
#[tauri::command(async, rename_all = "snake_case")]
fn stream_platforms() -> CmdResult<Vec<PlatformInfo>> {
    Ok(sf_ui_backend::platform_list())
}

/// A/V sync: automatic alignment on/off + manual offset (−100…+100 ms).
#[tauri::command(async, rename_all = "snake_case")]
fn av_sync_set(state: State<'_, AppState>, auto: bool, offset_ms: i32) -> CmdResult<AvSyncView> {
    Ok(lock(&state)?.av_sync_set(auto, offset_ms))
}

// ---------------------------------------------------------------------------
// Production screens: mixer
// ---------------------------------------------------------------------------

/// Sets a source channel gain (`0.0..=2.0`, clamped).
#[tauri::command(async, rename_all = "snake_case")]
fn mixer_set_gain(
    state: State<'_, AppState>,
    source_id: String,
    gain: f32,
) -> CmdResult<ChannelStrip> {
    lock(&state)?.mixer_set_gain(&source_id, gain).map_err(ue)
}

/// Mutes/unmutes a source channel.
#[tauri::command(async, rename_all = "snake_case")]
fn mixer_set_mute(
    state: State<'_, AppState>,
    source_id: String,
    mute: bool,
) -> CmdResult<ChannelStrip> {
    lock(&state)?.mixer_set_mute(&source_id, mute).map_err(ue)
}

/// Sets the master gain (`0.0..=2.0`, clamped).
#[tauri::command(async, rename_all = "snake_case")]
fn mixer_set_master_gain(state: State<'_, AppState>, gain: f32) -> CmdResult<ChannelStrip> {
    Ok(lock(&state)?.mixer_set_master_gain(gain))
}

/// Mutes/unmutes the master.
#[tauri::command(async, rename_all = "snake_case")]
fn mixer_set_master_mute(state: State<'_, AppState>, mute: bool) -> CmdResult<ChannelStrip> {
    Ok(lock(&state)?.mixer_set_master_mute(mute))
}

/// Enables/disables local speaker monitoring of the master mix.
#[tauri::command(async, rename_all = "snake_case")]
fn set_monitor_enabled(state: State<'_, AppState>, enabled: bool) -> CmdResult<bool> {
    lock(&state)?.set_monitor_enabled(enabled).map_err(ue)?;
    Ok(lock(&state)?.monitor_enabled())
}

/// Applies audio-enhancer settings (free offline DSP: gate → EQ → compressor).
#[tauri::command(async, rename_all = "snake_case")]
fn set_enhancer_settings(state: State<'_, AppState>, settings: EnhancerView) -> CmdResult<EnhancerView> {
    let mut s = lock(&state)?;
    s.set_enhancer_settings(settings);
    Ok(s.enhancer_settings())
}

// ---------------------------------------------------------------------------
// Production screens: streaming + recording (honest stubs)
// ---------------------------------------------------------------------------

/// Stores stream settings. `platform`: `youtube` | `facebook` | `tiktok` |
/// `custom_rtmp`. The key is kept in memory only — never logged, never
/// serialized, never in error strings.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn stream_start(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.stream_start().map_err(ue)
}

/// Stops streaming (any state → idle).
#[tauri::command(async, rename_all = "snake_case")]
fn stream_stop(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.stream_stop().map_err(ue)
}

/// Stores record settings. `format`: `mp4` | `mkv`.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn record_start(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_start().map_err(ue)
}

/// Stops recording (any state → idle).
#[tauri::command(async, rename_all = "snake_case")]
fn record_stop(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_stop().map_err(ue)
}

/// Pauses an active recording (elapsed time excludes pauses).
#[tauri::command(async, rename_all = "snake_case")]
fn record_pause(state: State<'_, AppState>) -> CmdResult<()> {
    lock(&state)?.record_pause().map_err(ue)
}

/// Resumes a paused recording.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
fn macro_add(
    state: State<'_, AppState>,
    name: String,
    hotkey: String,
    action: MacroAction,
) -> CmdResult<Macro> {
    lock(&state)?.macro_add(&name, &hotkey, action).map_err(ue)
}

/// Removes a macro by id.
#[tauri::command(async, rename_all = "snake_case")]
fn macro_remove(state: State<'_, AppState>, macro_id: String) -> CmdResult<()> {
    lock(&state)?.macro_remove(&macro_id).map_err(ue)
}

/// Lists all bound macros.
#[tauri::command(async, rename_all = "snake_case")]
fn macro_list(state: State<'_, AppState>) -> CmdResult<Vec<Macro>> {
    lock(&state).map(|s| s.macro_list())
}

/// Executes the macro bound to `hotkey`; reports what happened.
#[tauri::command(async, rename_all = "snake_case")]
fn macro_dispatch(state: State<'_, AppState>, hotkey: String) -> CmdResult<MacroDispatchResult> {
    lock(&state)?.macro_dispatch(&hotkey).map_err(ue)
}

/// Forwards one UI event to the frontend under the single `"sf-event"` channel.
fn forward(app: &AppHandle, event: &UiEvent) {
    let _ = app.emit("sf-event", event);
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState(Mutex::new(
            Shell::new().expect("engine must open a GPU adapter (headless: lavapipe)"),
        )))
        .setup(|app| {
            // Media pipeline: master tick at the Video panel's output fps
            // (30 → 33.3 ms, 60 → 16.7 ms). Renders one program frame and
            // advances record/stream encoders. Runs on its own thread so UI
            // polling, PNG previews, and transitions can never stall the
            // program feed. If the camera is late, the last frame is
            // repeated (tick never waits).
            //
            // OBS-style absolute-deadline clock (`video_sleep`): the tick
            // sleeps until the next whole-interval deadline instead of
            // sleeping a relative remainder, so the clock can never drift.
            // On overrun the clock jumps forward in whole intervals and the
            // missed ticks are counted as lagged (never burst-catch-up).
            let media_handle = app.handle().clone();
            std::thread::Builder::new()
                .name("sf-media-tick".into())
                .spawn(move || {
                    let mut next_deadline = std::time::Instant::now();
                    let mut missed_pending = 0u64;
                    loop {
                        // The tick carries its *scheduled* time: video PTS
                        // and the audio block length follow the wall clock
                        // even when a tick runs late.
                        let scheduled = sf_ui_backend::media_clock_us(next_deadline);
                        let interval_us =
                            if let Ok(mut s) = media_handle.state::<AppState>().0.lock() {
                                if missed_pending > 0 {
                                    s.perf_mark_lagged(missed_pending);
                                    missed_pending = 0;
                                }
                                // Never panics; failures degrade to Error states.
                                s.av_tick_at(scheduled);
                                s.frame_interval_us()
                            } else {
                                33_333
                            };
                        let interval_us = interval_us.max(1);
                        let interval = Duration::from_micros(interval_us);
                        next_deadline += interval;
                        let now = std::time::Instant::now();
                        if next_deadline > now {
                            std::thread::sleep(next_deadline - now);
                        } else {
                            // Missed: jump forward in whole intervals (never
                            // burst-catch-up; the skipped slots become a
                            // timestamp gap, not a backlog).
                            let over_us = now.duration_since(next_deadline).as_micros();
                            let missed = sf_ui_backend::missed_intervals(over_us, interval_us);
                            next_deadline += interval * missed as u32;
                            missed_pending += missed;
                        }
                    }
                })
                .expect("media tick thread");
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
            look_clear,
            filter_set_params,
            lut_import,
            lut_library,
            lut_remove,
            color_preset_save,
            color_preset_list,
            color_preset_apply,
            color_preset_delete,
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
            source_audio_device,
            video_settings_get,
            video_settings_set,
            camera_reconfigure,
            camera_sources,
            list_audio_devices,
            media_set_paused,
            perf_stats,
            perf_stages,
            stream_encoder_name,
            source_remove,
            source_reorder,
            source_set_visible,
            source_set_transform,
            scene_apply_layout,
            production_state,
            production_set_preview,
            production_set_program,
            transition_cut,
            transition_auto,
            scene_preview_jpg,
            monitor_frame_if_new,
            diagnostics,
            stream_platforms,
            av_sync_set,
            mixer_set_gain,
            mixer_set_mute,
            mixer_set_master_gain,
            mixer_set_master_mute,
            set_monitor_enabled,
            set_enhancer_settings,
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
