# Preset packs

Curated, ready-to-apply presets (spec "preset packs", shipped in C8).
Every file is validated by `crates/sf-engine/tests/packs_integration.rs`:
each `.sfstack` parses as a [`FilterStack`][1] and applies end-to-end in the
engine; each `.sflook` parses as a [`LookPreset`][2].

[1]: ../../crates/sf-filters/src/lib.rs
[2]: ../../crates/sf-filters/src/lib.rs

## Creator Starter Pack

- `creator-podcast.sfstack` — podcast / talking-head chain: **Podcast Warm**
  look (0.8) → subtle **Vignette** (0.35 × 0.6 slot) → **Skin Smooth** (0.6 × 0.7).
- `creator-vlog.sfstack` — vlog chain: **Vlog Fresh** look (0.9) → **Sharpen**
  (0.5 × 0.5).

## Cinematic Pack

- `cinematic-teal-orange.sfstack` — blockbuster chain: **Teal & Orange** look
  (1.0) → **Film Grain** (0.4 × 0.5) → **Vignette** (0.5 × 0.7) → **Letterbox**
  2.39:1.

## Community looks

- `golden-hour-glow.sflook` — warm sunset push (5200 K, warm gain), Creator.
- `arctic-clean.sflook` — cool crisp correction (7200 K), Natural.

All intensities are 0–100% slot/look strengths; every slot is bypassable
(`enabled` / intensity 0) per the spec's filter rules.
