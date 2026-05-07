use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Application configuration loaded from a TOML file.
///
/// All sections except `[auth]` are optional and have sensible defaults
/// so existing single-section `[auth]` configs continue to load without
/// modification.
///
/// CLI flags always override the values loaded here — see `main.rs`.
#[allow(dead_code)] // Several sections (server, capture, logging) are scaffolding
                    // for future blocks (D, E follow-ups) and intentionally
                    // exposed as public API even before they're wired in.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct AppConfig {
    /// Authentication settings.
    pub auth: AuthConfig,

    /// Optional audio output device name for loopback capture.
    /// On Windows: WASAPI render endpoint name (e.g. "Speakers (Realtek …)") or "default".
    /// On Linux: PulseAudio source name.
    /// Leave unset or empty to let the user choose from the browser toolbar.
    /// **Deprecated**: prefer `[audio].default_device`.  Kept for backward
    /// compatibility — when both are present the top-level value wins.
    #[serde(default)]
    pub audio_device: Option<String>,

    /// HTTP/WebSocket server settings.
    #[serde(default)]
    pub server: ServerSection,

    /// Screen-capture settings.
    #[serde(default)]
    pub capture: CaptureSection,

    /// Encoder defaults + named profiles.
    #[serde(default)]
    pub encoder: EncoderSection,

    /// Audio capture settings.
    #[serde(default)]
    pub audio: AudioSection,

    /// Logging settings.
    #[serde(default)]
    pub logging: LoggingSection,
}

/// Authentication credentials.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct AuthConfig {
    /// Login username.
    #[serde(default)]
    pub username: String,
    /// Login password (plain text in config, compared via constant-time hash).
    #[serde(default)]
    pub password: String,
}

/// `[server]` — listening address and static-file root.
#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)] pub struct ServerSection {
    /// Listen address (e.g. "0.0.0.0").  Falls back to CLI default when unset.
    #[serde(default)]
    pub host: Option<String>,
    /// Listen port (e.g. 9090).
    #[serde(default)]
    pub port: Option<u16>,
    /// Path to client static files.
    #[serde(default)]
    pub static_dir: Option<String>,
}

/// `[capture]` — screen-capture backend selection.
#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)] pub struct CaptureSection {
    /// Default monitor index to capture (0 = primary).  Client toolbar
    /// can switch at runtime.
    #[serde(default)]
    pub default_monitor: Option<u8>,
    /// Backend name: `"scrap"` (default, DXGI Desktop Duplication via the
    /// `scrap` crate), `"wgc"` (Windows.Graphics.Capture, stub) or
    /// `"dxgi"` (direct DXGI, reserved).
    #[serde(default)]
    pub capture_backend: Option<String>,
}

impl CaptureSection {
    /// Parsed backend kind.  Unknown values fall back to `Scrap`.
    #[allow(dead_code)] // wired by Block D capture trait integration
    pub fn backend_kind(&self) -> CaptureBackendKind {
        match self.capture_backend.as_deref().unwrap_or("scrap") {
            "wgc" | "WGC" => CaptureBackendKind::Wgc,
            "dxgi" | "DXGI" => CaptureBackendKind::Dxgi,
            _ => CaptureBackendKind::Scrap,
        }
    }
}

/// Capture-backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] pub enum CaptureBackendKind {
    /// `scrap` crate — DXGI Desktop Duplication, current default.
    Scrap,
    /// Windows.Graphics.Capture (stub for now).
    Wgc,
    /// Direct DXGI integration (reserved, not yet implemented).
    Dxgi,
}

/// `[encoder]` — default profile + named encoder profiles.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
#[allow(dead_code)]
pub struct EncoderSection {
    /// Name of the profile to use when the operator doesn't override
    /// at the CLI.  Must match a key under `[encoder.profiles.*]`.
    #[serde(default)]
    pub default_profile: Option<String>,
    /// When `true`, the server runs hardware-probing at startup and
    /// chooses an encoder automatically.  When `false`, the
    /// CLI/profile encoder is used as-is.
    #[serde(default = "default_true")]
    pub auto_select: bool,
    /// Named encoder profiles (e.g. `gaming`, `office`, `lan`).
    #[serde(default)]
    pub profiles: BTreeMap<String, EncoderProfile>,
}

fn default_true() -> bool {
    true
}

impl Default for EncoderSection {
    fn default() -> Self {
        Self {
            default_profile: None,
            auto_select: true,
            profiles: BTreeMap::new(),
        }
    }
}

/// One named encoder profile under `[encoder.profiles.<name>]`.
///
/// All fields are optional; the server merges them with CLI defaults.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct EncoderProfile {
    /// FFmpeg encoder name (e.g. `h264_amf`, `libsvtav1`).
    #[serde(default)]
    pub encoder: Option<String>,
    /// Codec family (`"h264"`, `"hevc"`, `"av1"`).  Auto-detected from
    /// `encoder` when omitted.
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub slices: Option<u32>,
    /// Chroma sub-sampling: `"420"` or `"444"`.
    #[serde(default)]
    pub chroma: Option<String>,
}

/// `[audio]` — audio capture defaults.
#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)] pub struct AudioSection {
    /// Default audio device (overrides legacy top-level `audio_device`
    /// when *both* are set the top-level wins for backward compat).
    #[serde(default)]
    pub default_device: Option<String>,
    /// Sample rate hint (Hz).  Currently only `48000` is wired through
    /// the rest of the pipeline; future encoders may use this.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Channel count hint (1 = mono, 2 = stereo).
    #[serde(default)]
    pub channels: Option<u8>,
}

/// `[logging]` — logging configuration.
#[derive(Debug, Deserialize, Default, Clone)]
#[allow(dead_code)] pub struct LoggingSection {
    /// Log level filter (`"trace"`, `"debug"`, `"info"`, `"warn"`,
    /// `"error"`).  Falls back to the `RUST_LOG` env var when unset.
    #[serde(default)]
    pub level: Option<String>,
    /// Optional log file path; when set, log lines are also written here.
    /// (File logging is best-effort and additional to stderr.)
    #[serde(default)]
    pub file: Option<String>,
}

impl AppConfig {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;
        let config: AppConfig = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config file '{}': {}", path.display(), e))?;

        if config.auth.username.is_empty() || config.auth.password.is_empty() {
            return Err("auth.username and auth.password must not be empty".into());
        }

        Ok(config)
    }

    /// Look up the named profile (or the configured `default_profile`
    /// when `name` is `None`).
    pub fn profile(&self, name: Option<&str>) -> Option<&EncoderProfile> {
        let key = name
            .map(|s| s.to_string())
            .or_else(|| self.encoder.default_profile.clone())?;
        self.encoder.profiles.get(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> AppConfig {
        toml::from_str(s).expect("must parse")
    }

    #[test]
    fn legacy_minimal_auth_only_config_still_parses() {
        let cfg = parse(r#"
            [auth]
            username = "alice"
            password = "secret"
        "#);
        assert_eq!(cfg.auth.username, "alice");
        assert!(cfg.audio_device.is_none());
        assert!(cfg.server.host.is_none());
        assert!(cfg.encoder.profiles.is_empty());
        assert!(cfg.encoder.auto_select); // defaults to true
        assert_eq!(cfg.capture.backend_kind(), CaptureBackendKind::Scrap);
    }

    #[test]
    fn legacy_top_level_audio_device_still_parses() {
        let cfg = parse(r#"
            audio_device = "Speakers (Realtek)"

            [auth]
            username = "u"
            password = "p"
        "#);
        assert_eq!(cfg.audio_device.as_deref(), Some("Speakers (Realtek)"));
    }

    #[test]
    fn full_config_parses_all_sections() {
        let cfg = parse(r#"
            [auth]
            username = "u"
            password = "p"

            [server]
            host = "127.0.0.1"
            port = 8443
            static_dir = "./static"

            [capture]
            default_monitor = 1
            capture_backend = "wgc"

            [encoder]
            default_profile = "gaming"
            auto_select = false

            [encoder.profiles.gaming]
            encoder = "h264_amf"
            codec = "h264"
            fps = 60
            quality = 18
            bitrate_kbps = 12000
            slices = 4
            chroma = "420"

            [encoder.profiles.office]
            encoder = "libx264"
            fps = 30
            quality = 24

            [audio]
            default_device = "default"
            sample_rate = 48000
            channels = 2

            [logging]
            level = "debug"
            file = "/tmp/srv.log"
        "#);

        assert_eq!(cfg.server.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(cfg.server.port, Some(8443));
        assert_eq!(cfg.capture.default_monitor, Some(1));
        assert_eq!(cfg.capture.backend_kind(), CaptureBackendKind::Wgc);

        assert_eq!(cfg.encoder.default_profile.as_deref(), Some("gaming"));
        assert!(!cfg.encoder.auto_select);
        assert_eq!(cfg.encoder.profiles.len(), 2);

        let gaming = cfg.encoder.profiles.get("gaming").expect("gaming present");
        assert_eq!(gaming.encoder.as_deref(), Some("h264_amf"));
        assert_eq!(gaming.fps, Some(60));
        assert_eq!(gaming.quality, Some(18));
        assert_eq!(gaming.bitrate_kbps, Some(12000));
        assert_eq!(gaming.slices, Some(4));
        assert_eq!(gaming.chroma.as_deref(), Some("420"));

        let office = cfg.encoder.profiles.get("office").expect("office present");
        assert_eq!(office.encoder.as_deref(), Some("libx264"));
        assert_eq!(office.fps, Some(30));

        assert_eq!(cfg.audio.default_device.as_deref(), Some("default"));
        assert_eq!(cfg.audio.sample_rate, Some(48000));
        assert_eq!(cfg.audio.channels, Some(2));

        assert_eq!(cfg.logging.level.as_deref(), Some("debug"));
        assert_eq!(cfg.logging.file.as_deref(), Some("/tmp/srv.log"));
    }

    #[test]
    fn profile_lookup_uses_default_when_no_name_given() {
        let cfg = parse(r#"
            [auth]
            username = "u"
            password = "p"

            [encoder]
            default_profile = "office"

            [encoder.profiles.office]
            encoder = "libx264"
        "#);
        let p = cfg.profile(None).expect("default profile resolves");
        assert_eq!(p.encoder.as_deref(), Some("libx264"));
    }

    #[test]
    fn profile_lookup_returns_none_for_unknown_profile() {
        let cfg = parse(r#"
            [auth]
            username = "u"
            password = "p"
        "#);
        assert!(cfg.profile(Some("missing")).is_none());
        assert!(cfg.profile(None).is_none());
    }

    #[test]
    fn capture_backend_unknown_value_falls_back_to_scrap() {
        let cfg = parse(r#"
            [auth]
            username = "u"
            password = "p"

            [capture]
            capture_backend = "magic"
        "#);
        assert_eq!(cfg.capture.backend_kind(), CaptureBackendKind::Scrap);
    }
}
