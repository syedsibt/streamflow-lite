# Camera capture audit (P0: lag, drops, "Waiting for frames", ~1.2 s age)

Scope: DirectShow camera → capture thread → mailbox → tick → render → monitor/encoder.
Evidence classes: **Confirmed** (source code / test), **Suspected** (consistent with
the evidence, not yet proven), **Unknown**.

## Pipeline and what is measured per stage (Diagnostics → Video sources)

| Stage | Metric | Source |
|---|---|---|
| Camera → driver | `Camera sends` (video packets / s) | `LiveVideoStats.device_fps` |
| Read + decode | `Decode` ms (EMA) | `drain_live` timing around send/receive |
| Pixel convert (→ RGBA) | `Convert` ms (EMA) | timing around `scale_picture` |
| Device timestamp → mailbox | `Camera → capture` ms | `capture_latency_us` |
| Mailbox publish rate | `Captured` fps (rolling 1 s) | `capture_fps` |
| Longest stall | `Longest gap (1 s)` ms | publish timestamps |
| Program pull | `Reaching program` fps | tick arrivals |
| Stale frames skipped | `Skipped (newer frame)` | newest-wins drops |
| Errors | decode / read errors | counters (no longer hidden by the next good frame) |
| Negotiation | format string + failed attempts | `DshowProducer::negotiated_format` |

An automatic verdict names the first stage that falls below 80 % of the nominal rate.

## Confirmed

1. **The MJPEG request was never applied.** FFmpeg's dshow takes the compressed mode
   from `AVFormatContext.video_codec_id` (`dshow_read_header`:
   `ctx->video_codec_id = avctx->video_codec_id ? … : AV_CODEC_ID_RAWVIDEO`), which the
   CLI's `-vcodec` sets. Passing `vcodec` in the options dictionary is silently ignored,
   so the "mjpeg" attempt was really "any codec". Fixed: the codec is set on the
   context; any option the input leaves unconsumed now fails the attempt.
2. **60 fps requests are rejected by most drivers.** `dshow_cycle_formats` converts the
   rate as `den*10000000/num` (integer) and requires it inside
   `[MinFrameInterval, MaxFrameInterval]`. `60` → 166666; drivers typically advertise
   166667. Every explicit 60 fps attempt failed and the app fell back to "device
   default" — the user's screenshot shows exactly `[via negotiated]`. Fixed: request the
   exact interval as a reduced `num:den` (`av_parse_video_rate` bound 1 001 000), then
   the plain rate. Unit-tested for 5…120 fps.
3. **Stats hid stalls.** `capture_fps` only updated on publish, so it froze at its last
   value; errors cleared on the next good frame. Fixed (rolling window, counters).
4. **The app's own path is fast on a 2-core VM** (release, synthetic 1080p60 MJPEG):
   decode ≈ 7 ms, convert ≈ 4 ms, device→mailbox ≈ 10 ms, ~31–33 fps published,
   longest gap 40–49 ms, 0 errors. CPU decode is not the bottleneck at this load.
5. **"Waiting for frames…" flapped** whenever the 1 s fps counter rounded to 0 (2–4 fps
   camera). Now shown only before the first frame; afterwards the real rate is shown.

## Suspected (needs the user's Diagnostics with v0.1.25+)

- The camera's *default* mode (what the fallback opened) delivers few fps — e.g. a raw
  1080p mode limited by USB 2 bandwidth, or a mode the driver throttles.
- Low light: many webcams lengthen exposure (auto-exposure / low-light compensation),
  dropping to 5–15 fps in a dark room (the screenshots are dark). The verdict line will
  say "the camera itself sends only N fps" in that case.
- Late ticks on the user's PC (80 "Late frames") — if the render loop is slow (e.g. a
  software GPU adapter), fewer camera frames reach the program even when capture is fine
  ("Reaching program" < "Captured").

## Unknown

- The user's real `Camera sends` rate, decode/convert times, and which attempt now
  opens (`[via mjpeg]` expected).
- GPU adapter in use on the user's PC.

## Before / after

| Metric | Before (user screenshot) | After |
|---|---|---|
| Negotiated | mjpeg 1080p60 `[via negotiated]` | Not measured on the user's camera |
| Camera fps reaching program | 1–4 | Not measured on the user's camera |
| Picture age | 380–1666 ms | Not measured on the user's camera |
| Dev VM, synthetic MJPEG 1080p60 | — | 31–33 fps published, ~10 ms capture latency |
| CPU / GPU / memory | Not measured | Not measured |

Nothing here is declared fixed on hardware until the Diagnostics screenshot from the
user's PC shows it.
