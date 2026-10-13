//! Streaming platform adapters (YouTube, Facebook, TikTok, custom RTMP).
//!
//! An adapter only turns the user's settings into a validated ingest URL
//! and platform guidance. It is deliberately isolated from the media core:
//! the engine streams one FLV/RTMP(S) feed and never knows which platform
//! it goes to. Adding a platform = one match arm here.
//!
//! The stream key never leaves this module except inside the final URL
//! handed to the send worker (which redacts it in every message).

use crate::production::{StreamPlatform, StreamSettings};

/// Resolved ingest target.
pub(crate) struct ResolvedTarget {
    /// `server/key` — secret, never logged.
    pub(crate) full_url: String,
}

impl std::fmt::Debug for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResolvedTarget({})", sf_output::redact_url(&self.full_url))
    }
}

/// Static info about a platform, for the Stream screen.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PlatformInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Default ingest server (`None` = the platform gives a per-stream URL).
    pub default_server: Option<&'static str>,
    /// Where to find the key / server in the platform's UI.
    pub key_help: &'static str,
    /// What the platform adds to end-to-end latency (outside the app).
    pub latency_note: &'static str,
}

/// Platforms offered in the UI.
pub fn platform_list() -> Vec<PlatformInfo> {
    [
        StreamPlatform::Youtube,
        StreamPlatform::Facebook,
        StreamPlatform::Tiktok,
        StreamPlatform::CustomRtmp,
    ]
    .into_iter()
    .map(platform_info)
    .collect()
}

pub fn platform_info(p: StreamPlatform) -> PlatformInfo {
    match p {
        StreamPlatform::Youtube => PlatformInfo {
            id: "youtube",
            name: "YouTube",
            default_server: Some("rtmp://a.rtmp.youtube.com/live2"),
            key_help: "YouTube Studio → Go live → Stream → Stream key",
            latency_note: "YouTube buffers on its side. In YouTube Studio → Stream settings choose “Ultra-low latency” for the smallest viewer delay (Normal latency adds the most).",
        },
        StreamPlatform::Facebook => PlatformInfo {
            id: "facebook",
            name: "Facebook",
            default_server: Some("rtmps://live-api-s.facebook.com:443/rtmp/"),
            key_help: "Facebook Live Producer → Streaming software → Stream key (Facebook requires RTMPS)",
            latency_note: "Facebook buffers on its side; choose the low-latency option in Live Producer for the smallest viewer delay.",
        },
        StreamPlatform::Tiktok => PlatformInfo {
            id: "tiktok",
            name: "TikTok",
            default_server: None,
            key_help: "TikTok LIVE Center / LIVE Studio → Server URL + Stream key (both change per stream)",
            latency_note: "TikTok buffers on its side; the viewer delay is set by TikTok, not by the app.",
        },
        StreamPlatform::CustomRtmp => PlatformInfo {
            id: "custom_rtmp",
            name: "Custom RTMP",
            default_server: None,
            key_help: "Server URL (rtmp:// or rtmps://) and key from your server",
            latency_note: "Server-side latency depends on your server/player.",
        },
    }
}

/// Validates the settings and builds the ingest URL.
pub(crate) fn resolve_target(s: &StreamSettings) -> Result<ResolvedTarget, String> {
    let info = platform_info(s.platform);
    let server = {
        let u = s.url.trim();
        if u.is_empty() {
            info.default_server
                .ok_or_else(|| match s.platform {
                    StreamPlatform::Tiktok => {
                        "TikTok needs the Server URL from TikTok LIVE Center — paste it in Stream settings".to_string()
                    }
                    _ => "no stream server URL set".to_string(),
                })?
                .to_string()
        } else {
            u.to_string()
        }
    };
    let lower = server.to_ascii_lowercase();
    if !(lower.starts_with("rtmp://") || lower.starts_with("rtmps://")) {
        return Err("the server URL must start with rtmp:// or rtmps://".to_string());
    }
    if server.chars().any(char::is_whitespace) {
        return Err("the server URL contains spaces".to_string());
    }
    if s.platform == StreamPlatform::Facebook && lower.starts_with("rtmp://") {
        return Err("Facebook only accepts RTMPS — use rtmps://live-api-s.facebook.com:443/rtmp/".to_string());
    }
    let key = s.key.trim();
    let key_required = !matches!(s.platform, StreamPlatform::CustomRtmp);
    if key.is_empty() && key_required {
        return Err(format!("no stream key set — {}", info.key_help));
    }
    // Never echo the key itself in an error.
    if key.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/') {
        return Err("the stream key contains invalid characters (spaces or '/') — paste it again".to_string());
    }
    if s.platform == StreamPlatform::Youtube
        && !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err("this does not look like a YouTube stream key (expected xxxx-xxxx-xxxx-xxxx-xxxx)".to_string());
    }
    let full_url = if key.is_empty() {
        server.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", server.trim_end_matches('/'), key)
    };
    Ok(ResolvedTarget { full_url })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(p: StreamPlatform, url: &str, key: &str) -> StreamSettings {
        StreamSettings {
            url: url.into(),
            key: key.into(),
            platform: p,
        }
    }

    #[test]
    fn youtube_default_server_and_key_validation() {
        let t = resolve_target(&st(StreamPlatform::Youtube, "", "abcd-efgh-ijkl-mnop-qrst")).unwrap();
        assert_eq!(t.full_url, "rtmp://a.rtmp.youtube.com/live2/abcd-efgh-ijkl-mnop-qrst");
        let e = resolve_target(&st(StreamPlatform::Youtube, "", "")).unwrap_err();
        assert!(e.contains("YouTube Studio"));
        let e = resolve_target(&st(StreamPlatform::Youtube, "", "abc def SECRET")).unwrap_err();
        assert!(!e.contains("SECRET"));
    }

    #[test]
    fn facebook_requires_rtmps() {
        let t = resolve_target(&st(StreamPlatform::Facebook, "", "FB-123-0-AbC")).unwrap();
        assert_eq!(t.full_url, "rtmps://live-api-s.facebook.com:443/rtmp/FB-123-0-AbC");
        assert!(resolve_target(&st(StreamPlatform::Facebook, "rtmp://x/rtmp", "k")).is_err());
    }

    #[test]
    fn tiktok_needs_server_url() {
        let e = resolve_target(&st(StreamPlatform::Tiktok, "", "k")).unwrap_err();
        assert!(e.contains("TikTok"));
        let t = resolve_target(&st(
            StreamPlatform::Tiktok,
            "rtmp://push.example.tiktokcdn.com/game/",
            "stream-123?expire=1",
        ))
        .unwrap();
        assert_eq!(t.full_url, "rtmp://push.example.tiktokcdn.com/game/stream-123?expire=1");
    }

    #[test]
    fn custom_validates_scheme() {
        assert!(resolve_target(&st(StreamPlatform::CustomRtmp, "http://x", "k")).is_err());
        assert!(resolve_target(&st(StreamPlatform::CustomRtmp, "", "k")).is_err());
        let t = resolve_target(&st(StreamPlatform::CustomRtmp, "rtmp://h/app", "")).unwrap();
        assert_eq!(t.full_url, "rtmp://h/app");
    }
}
