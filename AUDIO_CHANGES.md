# Audio: real-time AI enhancement + pipeline fixes

## What you get
* **AI noise removal (RNNoise)** on every mic / camera-audio source, live, with
  ON/OFF + strength (0–100 %) in the Audio view. Fans, hum, keyboard and room
  noise are removed; speech is kept. Music/media files are never processed.
* Voice-aware noise gate (the AI's voice detector holds the gate open for soft
  speech), 3-band EQ, compressor — all applied per voice source, pre-fader.
* Every slider change is heard on the next audio tick (≤ 33 ms).
* Per-mic live readout: device health, voice activity, gate, compressor gain
  reduction, added latency, queue depth, gaps.

## Why not a cloud API
A live stream needs each 33 ms audio block finished before the next arrives.
An HTTP API round-trip (100–500 ms + jitter) would delay the entire stream by
that much or drop audio; the cloud enhancement APIs I know of (e.g. Auphonic,
Dolby.io Media) process uploaded files, not live streams. Real-time products (Krisp, NVIDIA
Broadcast, OBS's RNNoise filter) all run the model locally — so does this.

## Bugs fixed (why audio "didn't work")
1. **Only the first chunk per source was mixed** each tick (`Mixer::mix` looks
   up one frame per id) — the rest were thrown away → choppy, garbled audio,
   wrong file playback speed. → `AudioBus` delivers every sample, in order.
2. **Ticks without a fresh chunk produced no audio** → the AAC timeline
   shrank against video PTS → growing A/V drift. → Exact samples every tick
   from a rational `SampleClock` (no rounding drift, 29.97 fps too).
3. **Mic reads blocked the media tick** (dshow waits up to 25 ms per read,
   called twice per tick) → tick overruns, skipped video frames, underruns.
   → `LiveInput` capture thread; the tick only drains a FIFO.
4. **DirectShow default ~500 ms audio buffer** → every word half a second
   late, in lumps. → `audio_buffer_size=20`.
5. **Monitor ring could grow to 2 s** of delay; on 44.1 kHz devices it cleared
   the ring after every callback (choppy). → 120 ms latency cap + fractional
   resampler.
6. **Audio track only created if a source existed at start** → a mic added
   mid-stream was never heard. → Audio track always present.
7. Monitoring only worked while live/recording → now also for mic checks.

## Verified here
* `sf-audio`: 41 unit tests pass (RNNoise port matches Xiph reference output;
  bus, clock, live-input thread, denoiser latency/strength, enhancer chain).
* Real-time harness (mic thread @ 10 ms packets, 30 fps tick, 5 s): exact
  240 000 frames out, 0 underruns, 0 drops, queue ≤ 43 ms, worst tick 3.9 ms.
* Full voice chain CPU: ~4.7 % of one core (21× faster than real time).
* Monitor ring logic: 4 tests pass.
* Frontend: type-checks (with module stubs).

## Not verified here (needs your Windows machine)
The desktop backend (`apps/desktop/backend`) could not be compiled in this
environment (its wgpu/ffmpeg/cpal crates come from crates.io, which was
blocked). Run `cargo test -p sf-ui-backend` and `cargo tauri dev` on Windows
and test with a real microphone.
