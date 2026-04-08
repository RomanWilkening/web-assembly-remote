use serde::Deserialize;
use std::path::Path;

/// Application configuration loaded from a TOML file.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// Authentication settings.
    pub auth: AuthConfig,
    /// Optional audio capture device name.
    /// On Windows this is a DirectShow device name (e.g. "Stereo Mix (Realtek …)").
    /// On Linux this is a PulseAudio source name (e.g. "default").
    /// Leave unset or empty to disable audio.
    #[serde(default)]
    pub audio_device: Option<String>,
}

/// Authentication credentials.
#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    /// Login username.
    pub username: String,
    /// Login password (plain text in config, compared via constant-time hash).
    pub password: String,
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
}
