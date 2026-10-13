# StreamFlow — Live latency, A/V sync, rendering & UI: audit and changes

This release combines three documents: the original source audit, the Color Studio requirements, and the "UI/UX, A/V Sync, Rendering & Final Production" requirements.

Everything below was inspected in the actual source and, wherever this Linux build machine allowed, **measured** rather than assumed. The last section lists what could not be verified here.

---

## 1. Root cause of the ~1-minute live delay

Two independent defects were found. Each one alone produces a delay that keeps growing.

### 1a. Stream timestamps counted frames, not time (encoder)

`VideoEncoder::encode_rgba` stamped every frame with `frame_index++`, and the caller's time was used only as the origin. The AAC timeline worked the same way, counting samples per tick.

Whenever the PC missed a tick (load, a device open, a GC pause in the WebView), the media clock of the stream fell behind real time and never caught up. Example: at 25 of 30 fps for 6 minutes, the stream carries only 5 minutes of media. The platform plays it at 1×, so viewers end up **1 minute behind and drifting further**.

**Fix**
- Video PTS is now the frame slot of the tick's *scheduled wall-clock time*, `round((t − origin) · fps)`. A skipped tick becomes a timestamp gap, not a shift.
- Audio block length = wall time elapsed since the previous tick (`sf_audio::AudioTimeline`).
- The media thread passes its absolute deadline (`av_tick_at`).

**Proof**
- `sf-encode`: `pts_follow_real_time_across_skipped_frames`.
- Backend end-to-end, `stream_and_record_share_one_encoder_and_follow_wall_clock`: real RTMP server, injected 400 ms stall, a 2-core VM running far below 30 fps. Received video timeline 5.07 s vs wall clock 5.13 s. The old code would have produced ~2.1 s.

### 1b. Camera capture never returned on a live device (capture)

ffmpeg-next's `packets()` iterator only ends on EOF and silently retries every other error. On a live dshow device it therefore never ends. The old "drain with deadline" loop could not return. The reproduction program in `scratchpad/oldbug` printed:

```
OLD drain loop: returned=false after 5.0s; packets read=171; interrupt callback calls=0
```

FFmpeg n6.1.1 source confirms that `dshow_read_packet` blocks in `WaitForMultipleObjects(INFINITE)` and ignores the interrupt callback. It returns `EAGAIN` only with `AVFMT_FLAG_NONBLOCK`.

**Fix: new `sf_capture::LiveVideo`**
- One capture thread per device, non-blocking reads (`AVFMT_FLAG_NONBLOCK`).
- Each pass drains everything queued and keeps only the newest picture. Intra codecs such as MJPEG decode only the newest packet; inter codecs decode all packets but show the newest.
- A 10 ms pass budget, a newest-frame mailbox, and dropped frames are counted.

**Tests** (real FFmpeg live sender over TCP):
- `live_mjpeg_never_builds_a_backlog`: after a 3 s consumer stall the next frame is < 150 ms old.
- `live_h264_decodes_references_but_shows_newest`.
- `drop_never_blocks`.

### 1c. Other latency/backlog sources found and fixed

| Where | Problem | Fix |
|---|---|---|
| Tauri adapter | All commands were sync, so they ran on the **UI/main thread**. The monitor was polled twice every 33 ms with no in-flight guard, so requests queued up. | Every command is `async` (off the main thread). New `monitor_frame_if_new` sends a frame only when it changed. Every frontend poll has an in-flight guard. |
| Monitor | JPEG encode under the shell lock. A "stale" fallback re-rendered the scene and **pulled producers again (stole camera frames)**. | Shared `Arc` frame taken in O(1); JPEG encoded *after* releasing the lock; on-demand renders never touch producers. |
| Render | Each consumer (program, preview scene, Color Studio, other scenes) pulled producers itself. Wrong-size cached frames could be returned. | **Per-tick source cache**: each producer is pulled exactly once per tick. Resizes are cached per (source, size, frame seq). |
| Render | A preview scene the UI no longer shows was rendered every tick. | Removed. Measured tick dropped from 93 ms to 64 ms on this VM. |
| Media files | `next_frame` cloned the full previous frame on every call. | Returns only new frames. |
| Recording + streaming | **Two encoders** encoded the same frame. | One encoder pair fans out to both. A sink that joins late gets a forced IDR and its own timestamp origin. |
| Encoder | B-frames = 2 (≈ 66 ms of reorder delay). NVENC was given x264 preset names: invalid since SDK 10, so it failed and silently fell back to a slower encoder. No CBR/VBV. | `bf=0`. Per-encoder low-latency options (below). CBR with a 1 s VBV. |
| RTMP | No socket timeouts: a black-holed network wedged the send thread forever. Non-interleaved writes. `stream_stop` could block the UI for 5 s. | Interrupt-callback deadlines (connect 10 s, write 5 s, close 2 s), `rw_timeout`, `tcp_nodelay`, interleaved FLV writes, detached stop. Test `connect_to_silent_server_times_out`. |
| Audio input | Dropping a `LiveInput` *joined* its thread, which hangs the UI on an unplugged device. | Detached stop. |
| Diagnostics | "CPU %" was tick time divided by wall time (not CPU). | Real process CPU (Windows `GetProcessTimes`, Linux `/proc`) and RAM. |

---

## 2. Stage-by-stage latency (measured)

Measured with `latency_bench`, a release build:
- Pipeline: media 1080p30 → GPU color → libx264 → RTMP to a local FFmpeg receiver.
- 20 s run with 500 ms stalls injected every 5 s.
- **Hardware: a 2-core cloud VM with *software* OpenGL (llvmpipe), no GPU encoder.** These numbers are a worst case, not what a gaming/streaming PC will show.

| Stage (per frame, avg) | ms |
|---|---|
| Capture (decode media file + RGBA) | 4.97 |
| Render: grade + composite (software GPU) | 10.2 |
| Audio (mix + enhancer) | 4.9 |
| Encode + send (x264 1080p on 2 CPU cores) | 26.3 |
| **Whole tick** | 64.4 (max 189) |

**Delay growth**

Packet arrival time vs media timestamp, relative to the first packet:
- First half of the run: median −12 ms.
- Second half: median −17 ms (p95 27 ms).

**The delay did not grow at all.** This held even though the VM could only render ~15 of 30 fps: 327 late ticks were skipped, never queued.

**App side vs platform side**

The Diagnostics sheet now splits latency:
- **App side (measured live):**
  - Capture → render (from the device's own timestamps, mapped onto one monotonic clock with an OBS-style min-offset mapper).
  - Processing (render + encode).
  - Send buffer (encoded video not yet on the socket).
- **Platform side (outside the app):** ingest, transcode and player buffering. The app shows guidance per platform. For YouTube, *Ultra-low latency* in YouTube Studio is the setting that matters; *Normal latency* is where most of a large viewer delay comes from.

The sub-second target applies to the app side. On a normal PC it is the capture age (one camera frame ≈ 33 ms plus the device's internal latency) plus a few ms of processing and encode.

---

## 3. Single source of truth for frames

```
capture thread (newest only) ─► source cache (1 pull / source / tick)
                                   │
                                   ▼
                         grade + composite (GPU), once
                                   │ program frame
              ┌────────────────────┼─────────────────────┐
              ▼                    ▼                     ▼
   monitor (GPU-downscaled)   Color Studio tap      ONE encoder pair
                              (same graded frame)   ├─► recorder (rebased)
                                                    └─► RTMP send thread
```

- Color is applied before encoding.
- The preview, the Color Studio "After" view and the stream are the same pixels.
- A single opaque layer skips the composite entirely: the graded frame is moved, not copied.

---

## 4. A/V sync: automatic by default

- **Timestamps.** Every audio chunk and video frame carries its capture time on one monotonic pipeline clock. Device timestamps (dshow reference clock, audio device clock) are mapped through a `ClockMapper`: minimum-offset anchor, 200 ppm drift tracking, 2 s re-anchor.
- **Alignment.** Each live audio source is played at the **measured age of its picture**:
  - Camera audio uses the age of its own camera.
  - An external mic uses the age of the program's main camera.
  - The user's offset is added on top.
- **Corrections.** Fresher audio is delayed by holding it in the bus. Stale audio is dropped. There is a ±15 ms dead band, so steady sources are never touched, and a maximum added delay of 800 ms.
- **Tests:**
  - `aligns_audio_to_video_age`: a mic 80 ms fresher than the picture settles at the target age.
  - A 500 ms backlog is dropped back to the target.
  - `head_capture_time_tracks_queue_and_drops`.
- **Manual compensation (secondary):** an offset of −100…+100 ms in Audio → A/V sync.
- **Camera ↔ audio pairing** is now name-based only. For example, "Elgato HD60 X" pairs with "Elgato HD60 X Audio", and "Logitech BRIO" with "Microphone (Logitech BRIO)". Previously a camera grabbed *the first microphone* when nothing matched, which doubled the audio when that mic was also added on its own. Test: `pairs_capture_card_audio_by_name_only`.

The four requested combinations map to:

| Combination | How it is aligned |
|---|---|
| Camera + camera mic | Camera audio aligned to its own camera's age |
| Camera + external mic | Mic aligned to the program camera's age |
| Capture card video + audio | Card audio auto-paired by name, aligned to the card's video age |
| Capture card + external mic | Mic aligned to the card's video age; card audio as above (or set to "No audio") |

---

## 5. Encoder / FFmpeg settings (before → after)

| Setting | Before | After |
|---|---|---|
| PTS | frame counter | wall-clock frame slot |
| B-frames | 2 | 0 (all encoders) |
| GOP | 2 s | 2 s (platform requirement) |
| Rate control | bitrate only | CBR, `maxrate = minrate = bitrate`, `bufsize` = 1 s |
| x264 | veryfast, zerolatency | veryfast, zerolatency, high profile, forced-IDR, `nal-hrd=cbr` |
| NVENC | `preset=veryfast` (invalid) | `p4`, `tune=ll`, `rc=cbr`, `zerolatency`, `delay=0`, `rc-lookahead=0`, forced-IDR |
| QSV | — | veryfast, `async_depth=1`, `look_ahead=0`, `low_delay_brc`, forced IDR |
| AMF | — | `usage=lowlatency`, `rc=cbr`, `quality=speed`, forced IDR |
| Fallback | NVENC → QSV → AMF → x264 → mpeg4 | same, and the reason for a fallback is shown in Diagnostics |
| Encodes per frame (stream + record) | 2 | 1 |

`no_encoder_delay` verifies that x264 returns each frame's packet in the same call.

---

## 6. Platforms

The adapters live in `backend/src/platforms.rs`, isolated from the media core:

| Platform | Server | Validation |
|---|---|---|
| YouTube | `rtmp://a.rtmp.youtube.com/live2` | key format check |
| Facebook | `rtmps://live-api-s.facebook.com:443/rtmp/` | RTMPS enforced |
| TikTok | Server URL pasted from LIVE Center (it changes per stream) | required |
| Custom | any rtmp:// or rtmps:// URL | scheme check |

- Twitch was removed, as requested.
- The stream key is never stored, logged or echoed.
- Errors are scrubbed of the URL and key (tests in `platforms.rs` and `streamer.rs`).

---

## 7. Diagnostics

**Status bar** (always visible): VIDEO · AUDIO · COLOR · FPS · DROPPED · LATENCY · STREAM.

**Sheet**, all measured:
- Latency split (app vs platform).
- Per-stage bars against the frame budget.
- Output fps, late frames, encode failures and network drops.
- Per-source capture fps, device fps, skipped older frames, age and errors.
- Per-mic buffer, audio age vs target, and sync corrections.
- Encoder name, hardware/software, encode ms, fps and fallback note.
- Stream kbps and uptime.
- Process CPU, RAM, and GPU color time.

---

## 8. UI

The UI is rebuilt as **INPUT → PREVIEW → COLOR → AUDIO → STREAM**: dark only, iOS-style grouped sections, segmented controls, switches and thin sliders, without glassmorphism.

**Removed dead or extra UI:**
- Scenes panel, multiview, transitions bar, macros, the separate "Video" screen, and the A/B bus.
- The fake "Camera 1 (default)" color target.

**Overflow and window sizes.** All names, device strings and paths use single-line ellipsis with the full text in a tooltip. Errors wrap and scroll inside a dismissible banner. Checked with an automated overflow audit and screenshots at:
- 720×520 (narrow layout stacks).
- 1024×700 and 1440×900.
- 3840×2160 (scaled).

All of these used long device names, a long Windows path and long error messages.

**Graceful errors:**
- Device not found or unplugged: the source row shows the error and the A/V status turns red.
- Encoder fallback: the note is shown.
- Invalid or missing key, wrong scheme, Facebook without RTMPS, TikTok without a server: clear messages before connecting.
- Network failure: timeouts lead to reconnect, then an error banner.
- Invalid LUT: the existing validation.

---

## 9. Test matrix

**Legend**
- **auto** = covered by an automated test run here.
- **logic** = code path type-checked on Windows paths (`--cfg sf_wincheck`) and unit-tested, but needs real hardware.
- **HW** = must be run on the Windows PC.

### Inputs

| | Combination | Here | On the PC |
|---|---|---|---|
| A | Camera + camera mic | logic (pairing, alignment, capture thread) | HW: check the "AUDIO" age matches the camera age in Diagnostics |
| B | Camera + external mic | logic | HW: clap test |
| C | Capture card video + audio | logic | HW |
| D | Capture card + external mic | logic | HW |

### Color pipeline (auto: GPU path on software OpenGL)

| | Setup | Status |
|---|---|---|
| E | Color off | auto |
| F | Basic grade | auto (sf-render, sf-engine tests) |
| G | LUT | auto |
| H | LUT + filter | auto |
| I | LUT + color + glow | auto (shader validation from the Color Studio release) |

### Streaming

| | Platform | Here | On the PC |
|---|---|---|---|
| J | YouTube | auto to a local RTMP server; key/URL validation auto | HW: real ingest |
| K | Facebook | URL/RTMPS validation auto | HW: RTMPS needs an FFmpeg build with TLS |
| L | TikTok | server/key validation auto | HW |

For each run on the PC, read FPS, DROPPED, LATENCY, CPU, RAM and GPU from the Diagnostics sheet. The automated runs here verified fps accounting, drop accounting, wall-clock timelines and no delay growth.

**Automated test counts in this build:**

| Crate | Tests |
|---|---|
| sf-capture | 30 |
| sf-encode | 16 |
| sf-output | 10 |
| sf-audio | 46 |
| sf-render | 6 |
| backend unit | 24 |
| backend integration | 8 + 17 + 15, plus the ignored `latency_bench` |

All pass. The frontend type-checks against the real React 19 types.

---

## 10. Windows CI (GitHub's Windows Server 2022 runner)

The repository's Windows workflow was run on this branch. It builds the
real Tauri app (NSIS + MSI installers, portable exe) and runs the whole
workspace test suite on Windows with FFmpeg 6.1.1 and Microsoft's
software GPU (WARP):

- **363 tests pass on Windows**, including the live-capture backlog tests,
  the end-to-end RTMP stream + late-joining recording, the A/V aligner
  and every GPU color test.
- Windows-only issues found and fixed through it: DirectShow device
  enumeration was not safe to call concurrently (now serialized); tests
  that used `/tmp`, fake device names or Linux timing were made portable.
- The runner has no camera, microphone or NVIDIA/Intel/AMD GPU, so those
  still need the real PC.

## 11. Not verified here (honest)

- **No Windows capture hardware** (the Windows build and test suite *do* run in CI, see §10):
  - DirectShow cameras, capture cards and microphones.
  - NVENC / QSV / AMF option acceptance.
  - Real YouTube / Facebook / TikTok ingest.
  - The Tauri app itself. GTK/WebKit are not available here; the adapter was type-checked against an API shim, and the UI was rendered in Chromium with a mock backend.
- **Adding a camera while live** opens the device under the engine lock. A slow driver can pause output for the open time (bounded at 8 s). Thanks to the time-based clock this is a gap, not a delay, but it is visible.
- **Media files** are decoded on the tick thread (~5 ms per 1080p frame measured). Live devices decode on their own threads.
- **RTMPS (Facebook)** requires an FFmpeg build with TLS (schannel or OpenSSL). Standard Windows FFmpeg builds include it.
- A stray 970 KB file named `--list` was in the uploaded repository root (a gen-lut output written under the wrong name). It is left out of this zip.
