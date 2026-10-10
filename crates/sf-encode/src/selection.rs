//! Encoder selection: priority-ordered probing of the FFmpeg registry.
//!
//! The selection *logic* is pure — [`select_video_encoder`] takes an
//! injectable `available` probe — so it is unit-testable without FFmpeg.
//! [`ffmpeg_encoder_available`] is the real probe against the FFmpeg encoder
//! registry; [`select_system_video_encoder`] combines the two.

/// Video encoder choice, in priority order (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderId {
    /// NVIDIA NVENC H.264 (Windows, NVIDIA GPU).
    H264Nvenc,
    /// Intel Quick Sync H.264 (Windows, Intel iGPU).
    H264Qsv,
    /// AMD AMF H.264 (Windows, AMD GPU).
    H264Amf,
    /// Software x264 — always available wherever libx264 is linked.
    H264Libx264,
    /// MPEG-4 Part 2 — last resort; native FFmpeg encoder, always present.
    Mpeg4,
}

impl EncoderId {
    /// FFmpeg encoder name as found in the encoder registry.
    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            EncoderId::H264Nvenc => "h264_nvenc",
            EncoderId::H264Qsv => "h264_qsv",
            EncoderId::H264Amf => "h264_amf",
            EncoderId::H264Libx264 => "libx264",
            EncoderId::Mpeg4 => "mpeg4",
        }
    }

    /// True for GPU-backed encoders.
    pub fn is_hardware(&self) -> bool {
        matches!(
            self,
            EncoderId::H264Nvenc | EncoderId::H264Qsv | EncoderId::H264Amf
        )
    }
}

/// Pick the best available encoder in priority order
/// (nvenc → qsv → amf → libx264 → mpeg4).
///
/// `available` maps an FFmpeg encoder name to whether it exists in the
/// registry. Because mpeg4 is a native FFmpeg encoder, the final fallback
/// always hits in practice; the `unwrap_or` is belt-and-braces.
pub fn select_video_encoder(available: &dyn Fn(&str) -> bool) -> EncoderId {
    const ORDER: [EncoderId; 5] = [
        EncoderId::H264Nvenc,
        EncoderId::H264Qsv,
        EncoderId::H264Amf,
        EncoderId::H264Libx264,
        EncoderId::Mpeg4,
    ];
    ORDER
        .into_iter()
        .find(|id| available(id.ffmpeg_name()))
        .unwrap_or(EncoderId::Mpeg4)
}

/// Real availability probe: does the FFmpeg encoder registry contain `name`?
pub fn ffmpeg_encoder_available(name: &str) -> bool {
    ffmpeg_next::encoder::find_by_name(name).is_some()
}

/// Select the best encoder actually present on this machine.
pub fn select_system_video_encoder() -> EncoderId {
    // Ensure the registry is populated before probing.
    let _ = ffmpeg_next::init();
    select_video_encoder(&ffmpeg_encoder_available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn probe_of(names: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        move |name: &str| set.contains(name)
    }

    #[test]
    fn prefers_nvenc_when_everything_available() {
        let p = probe_of(&["h264_nvenc", "h264_qsv", "h264_amf", "libx264", "mpeg4"]);
        assert_eq!(select_video_encoder(&p), EncoderId::H264Nvenc);
    }

    #[test]
    fn all_hw_missing_falls_back_to_libx264() {
        let p = probe_of(&["libx264", "mpeg4"]);
        assert_eq!(select_video_encoder(&p), EncoderId::H264Libx264);
    }

    #[test]
    fn only_qsv_available_selects_qsv() {
        let p = probe_of(&["h264_qsv", "mpeg4"]);
        assert_eq!(select_video_encoder(&p), EncoderId::H264Qsv);
    }

    #[test]
    fn nothing_available_falls_back_to_mpeg4() {
        let p = probe_of(&[]);
        assert_eq!(select_video_encoder(&p), EncoderId::Mpeg4);
    }

    #[test]
    fn amf_beats_libx264_but_loses_to_qsv() {
        let p = probe_of(&["h264_amf", "h264_qsv", "libx264", "mpeg4"]);
        assert_eq!(select_video_encoder(&p), EncoderId::H264Qsv);
        let p = probe_of(&["h264_amf", "libx264", "mpeg4"]);
        assert_eq!(select_video_encoder(&p), EncoderId::H264Amf);
    }

    #[test]
    fn hardware_flag_matches_variant() {
        assert!(EncoderId::H264Nvenc.is_hardware());
        assert!(EncoderId::H264Qsv.is_hardware());
        assert!(EncoderId::H264Amf.is_hardware());
        assert!(!EncoderId::H264Libx264.is_hardware());
        assert!(!EncoderId::Mpeg4.is_hardware());
    }

    /// Hits the real FFmpeg registry on this machine.
    ///
    /// Honest finding: this VM's FFmpeg build *lists* `h264_nvenc` in its
    /// registry (compiled in) even though no NVIDIA GPU exists — registry
    /// presence does not imply usability. Selection therefore reports Nvenc,
    /// and `VideoEncoder::open` must fall back to an encoder that opens.
    #[test]
    fn real_registry_probe_finds_software_encoders() {
        let _ = ffmpeg_next::init();
        assert!(
            ffmpeg_encoder_available("libx264"),
            "system ffmpeg must have libx264"
        );
        assert!(ffmpeg_encoder_available("mpeg4"));
        assert!(!ffmpeg_encoder_available("definitely_not_a_real_encoder"));
        let id = select_system_video_encoder();
        eprintln!("selected on this machine: {id:?} ({})", id.ffmpeg_name());
        // Whatever is selected must actually open (fallback logic in
        // VideoEncoder::open); on this GPU-less VM that means libx264.
        let enc = crate::VideoEncoder::open(crate::VideoEncoderOpts {
            width: 64,
            height: 64,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 500_000,
            gop: 30,
            preset: "ultrafast".to_string(),
        })
        .expect("open with fallback");
        eprintln!("actually opened: {:?}", enc.encoder_id());
        assert_eq!(enc.encoder_id(), EncoderId::H264Libx264);
        // The pure selection (registry-only) picks nvenc here — documenting
        // that selection != usability.
        assert_eq!(id, EncoderId::H264Nvenc);
    }
}
