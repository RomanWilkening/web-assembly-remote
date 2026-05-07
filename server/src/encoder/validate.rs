//! Pre-flight validation of an [`EncoderConfig`] against the chosen
//! backend's [`BackendCaps`].
//!
//! Validation is performed *before* FFmpeg is spawned so the operator
//! gets a precise error message instead of a cryptic "FFmpeg exited
//! with status 1" plus stderr noise.

use super::backends::{lookup, BackendCaps};
use super::{Chroma, EncoderConfig};

/// Reason an [`EncoderConfig`] cannot be used as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// `quality` is outside the encoder's documented range.
    QualityOutOfRange { value: u8, min: u8, max: u8, encoder: String },
    /// `chroma = Yuv444` requested but the backend cannot ingest it.
    ChromaUnsupported { encoder: String },
    /// `slices > 1` requested but the backend ignores `-slices`.
    SlicesUnsupported { encoder: String },
    /// `bitrate_kbps == Some(0)` (would divide by zero in vbv calc).
    BitrateZero,
    /// `fps == 0` — would cause divide-by-zero in vbv buffer sizing.
    FpsZero,
    /// `width == 0 || height == 0`.
    ZeroResolution,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QualityOutOfRange { value, min, max, encoder } => write!(
                f,
                "quality={value} is outside {encoder}'s range [{min}, {max}]",
            ),
            Self::ChromaUnsupported { encoder } => write!(
                f,
                "encoder {encoder} does not support 4:4:4 chroma",
            ),
            Self::SlicesUnsupported { encoder } => write!(
                f,
                "encoder {encoder} ignores --slices > 1",
            ),
            Self::BitrateZero => write!(f, "--bitrate-kbps must be > 0 when set"),
            Self::FpsZero => write!(f, "fps must be > 0"),
            Self::ZeroResolution => write!(f, "width and height must both be > 0"),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Look up the caps for the encoder named in `cfg` (falling back to a
/// permissive default when it is unknown — for unknown encoders we let
/// FFmpeg arbitrate).
pub fn caps_for(cfg: &EncoderConfig) -> BackendCaps {
    lookup(&cfg.encoder_name)
        .map(|b| b.caps)
        .unwrap_or(BackendCaps::unknown())
}

/// Run all pre-flight checks against `cfg`.  Returns the matched
/// `BackendCaps` on success.
pub fn validate(cfg: &EncoderConfig) -> Result<BackendCaps, ValidationError> {
    if cfg.width == 0 || cfg.height == 0 {
        return Err(ValidationError::ZeroResolution);
    }
    if cfg.fps == 0 {
        return Err(ValidationError::FpsZero);
    }
    if let Some(0) = cfg.bitrate_kbps {
        return Err(ValidationError::BitrateZero);
    }

    let caps = caps_for(cfg);

    if cfg.quality < caps.min_quality || cfg.quality > caps.max_quality {
        return Err(ValidationError::QualityOutOfRange {
            value: cfg.quality,
            min: caps.min_quality,
            max: caps.max_quality,
            encoder: cfg.encoder_name.clone(),
        });
    }
    if cfg.chroma == Chroma::Yuv444 && !caps.supports_yuv444 {
        return Err(ValidationError::ChromaUnsupported {
            encoder: cfg.encoder_name.clone(),
        });
    }
    if cfg.slices > 1 && !caps.supports_slices {
        return Err(ValidationError::SlicesUnsupported {
            encoder: cfg.encoder_name.clone(),
        });
    }
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::CodecKind;

    fn cfg(encoder: &str) -> EncoderConfig {
        EncoderConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            quality: 20,
            encoder_name: encoder.to_string(),
            codec: CodecKind::from_encoder_name(encoder),
            chroma: Chroma::Yuv420,
            slices: 1,
            bitrate_kbps: None,
        }
    }

    #[test]
    fn validate_accepts_default_config() {
        for enc in ["h264_amf", "hevc_amf", "av1_amf", "h264_nvenc",
                    "hevc_nvenc", "av1_nvenc", "h264_qsv", "hevc_qsv",
                    "av1_qsv", "h264_mf", "hevc_mf", "libx264", "libx265",
                    "libsvtav1", "libaom-av1"] {
            let mut c = cfg(enc);
            // Media Foundation uses 0..100 instead of 0..51 — quality 20 is
            // valid in both.
            c.quality = 20;
            assert!(validate(&c).is_ok(), "encoder {enc} rejected default config: {:?}", validate(&c));
        }
    }

    #[test]
    fn validate_rejects_zero_resolution() {
        let mut c = cfg("libx264");
        c.width = 0;
        assert_eq!(validate(&c), Err(ValidationError::ZeroResolution));
        c.width = 1920;
        c.height = 0;
        assert_eq!(validate(&c), Err(ValidationError::ZeroResolution));
    }

    #[test]
    fn validate_rejects_fps_zero() {
        let mut c = cfg("libx264");
        c.fps = 0;
        assert_eq!(validate(&c), Err(ValidationError::FpsZero));
    }

    #[test]
    fn validate_rejects_bitrate_zero() {
        let mut c = cfg("libx264");
        c.bitrate_kbps = Some(0);
        assert_eq!(validate(&c), Err(ValidationError::BitrateZero));
    }

    #[test]
    fn validate_rejects_yuv444_on_av1_amf() {
        let mut c = cfg("av1_amf");
        c.chroma = Chroma::Yuv444;
        match validate(&c) {
            Err(ValidationError::ChromaUnsupported { .. }) => {}
            other => panic!("expected ChromaUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_slices_on_av1_amf() {
        let mut c = cfg("av1_amf");
        c.slices = 4;
        match validate(&c) {
            Err(ValidationError::SlicesUnsupported { .. }) => {}
            other => panic!("expected SlicesUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_quality_above_max_for_h264() {
        let mut c = cfg("libx264");
        c.quality = 60; // out of [0, 51]
        match validate(&c) {
            Err(ValidationError::QualityOutOfRange { value: 60, max: 51, .. }) => {}
            other => panic!("expected QualityOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_quality_in_range_for_av1() {
        let mut c = cfg("libsvtav1");
        c.quality = 60; // valid for AV1 (0..63)
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn unknown_encoder_uses_permissive_defaults() {
        let c = cfg("magic_new_encoder");
        assert!(validate(&c).is_ok());
    }
}
