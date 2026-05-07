//! Per-encoder argument builders + capability tables.
//!
//! Each submodule describes one *family* of FFmpeg encoders (AMF, NVENC,
//! QSV, Media Foundation, software).  The `BackendCaps` constant exposes
//! what the family supports; `build_args` extends an in-progress
//! `Command` with the codec-specific FFmpeg flags.
//!
//! Splitting the giant `match cfg.encoder_name` block from the original
//! `encoder.rs` into one module per backend means each variant can be
//! unit-tested in isolation and a new variant can be added without
//! touching code that already works.

use super::{Chroma, EncoderConfig};
use std::process::Command;

pub mod amf;
pub mod nvenc;
pub mod qsv;
pub mod mediafoundation;
pub mod software;
pub mod generic;

/// Hardware vendor a backend is designed for.  Used by hardware-probing
/// to prefer encoders that match the GPU actually present in the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwVendor {
    Amd,
    Nvidia,
    Intel,
    Microsoft,
    Software,
    Other,
}

impl HwVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            HwVendor::Amd => "amd",
            HwVendor::Nvidia => "nvidia",
            HwVendor::Intel => "intel",
            HwVendor::Microsoft => "microsoft",
            HwVendor::Software => "software",
            HwVendor::Other => "other",
        }
    }
}

/// Static capability description for one encoder family.
///
/// All numeric ranges are *inclusive* on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    /// Vendor of the hardware this backend targets (or `Software`).
    pub vendor: HwVendor,
    /// True if the encoder accepts `-pix_fmt yuv444p` from FFmpeg.
    pub supports_yuv444: bool,
    /// True if the encoder honours the codec-generic `-slices N` flag.
    pub supports_slices: bool,
    /// Inclusive QP / CRF range exposed by FFmpeg for this encoder
    /// (used in pre-flight validation).
    pub min_quality: u8,
    pub max_quality: u8,
}

impl BackendCaps {
    /// Conservative default for unknown encoders.
    pub const fn unknown() -> Self {
        Self {
            vendor: HwVendor::Other,
            supports_yuv444: false,
            supports_slices: false,
            min_quality: 0,
            max_quality: 51,
        }
    }
}

/// Result of looking up a backend by FFmpeg encoder name.
pub struct Backend {
    pub name: &'static str,
    pub caps: BackendCaps,
    pub build_args: fn(&mut Command, &EncoderConfig, &RcArgs),
}

/// Pre-computed rate-control argument strings shared by every backend.
///
/// `quality` and `bitrate_kbps`/`vbv_bufsize` are formatted once in
/// `mod.rs` and passed into each `build_args` so the per-backend code
/// stays focused on the *shape* of the flag list, not the formatting.
pub struct RcArgs {
    pub quality: String,
    pub slices: String,
    pub want_slices: bool,
    pub bitrate_arg: Option<String>,
    pub vbv_buf_arg: Option<String>,
}

impl RcArgs {
    pub fn from_config(cfg: &EncoderConfig) -> Self {
        let slices_n = cfg.slices.max(1);
        Self {
            quality: cfg.quality.to_string(),
            slices: slices_n.to_string(),
            want_slices: slices_n > 1,
            bitrate_arg: cfg.bitrate_kbps.map(|kb| format!("{kb}k")),
            vbv_buf_arg: cfg
                .bitrate_kbps
                .map(|kb| format!("{}", (kb * 1000) / cfg.fps.max(1))),
        }
    }
}

/// Look up a backend by its FFmpeg encoder name.  Returns `None` for
/// fully unknown names (the caller falls back to a generic builder that
/// just sets `-c:v <name>` and an optional bitrate).
pub fn lookup(name: &str) -> Option<Backend> {
    match name {
        "h264_amf" => Some(Backend {
            name: "h264_amf",
            caps: amf::CAPS_H264,
            build_args: amf::build_h264,
        }),
        "hevc_amf" => Some(Backend {
            name: "hevc_amf",
            caps: amf::CAPS_HEVC,
            build_args: amf::build_hevc,
        }),
        "av1_amf" => Some(Backend {
            name: "av1_amf",
            caps: amf::CAPS_AV1,
            build_args: amf::build_av1,
        }),
        "h264_nvenc" => Some(Backend {
            name: "h264_nvenc",
            caps: nvenc::CAPS_H264,
            build_args: nvenc::build_h264,
        }),
        "hevc_nvenc" => Some(Backend {
            name: "hevc_nvenc",
            caps: nvenc::CAPS_HEVC,
            build_args: nvenc::build_hevc,
        }),
        "av1_nvenc" => Some(Backend {
            name: "av1_nvenc",
            caps: nvenc::CAPS_AV1,
            build_args: nvenc::build_av1,
        }),
        "h264_qsv" => Some(Backend {
            name: "h264_qsv",
            caps: qsv::CAPS_H264,
            build_args: qsv::build_h264,
        }),
        "hevc_qsv" => Some(Backend {
            name: "hevc_qsv",
            caps: qsv::CAPS_HEVC,
            build_args: qsv::build_hevc,
        }),
        "av1_qsv" => Some(Backend {
            name: "av1_qsv",
            caps: qsv::CAPS_AV1,
            build_args: qsv::build_av1,
        }),
        "h264_mf" => Some(Backend {
            name: "h264_mf",
            caps: mediafoundation::CAPS_H264,
            build_args: mediafoundation::build_h264,
        }),
        "hevc_mf" => Some(Backend {
            name: "hevc_mf",
            caps: mediafoundation::CAPS_HEVC,
            build_args: mediafoundation::build_hevc,
        }),
        "libx264" => Some(Backend {
            name: "libx264",
            caps: software::CAPS_X264,
            build_args: software::build_x264,
        }),
        "libx265" => Some(Backend {
            name: "libx265",
            caps: software::CAPS_X265,
            build_args: software::build_x265,
        }),
        "libsvtav1" => Some(Backend {
            name: "libsvtav1",
            caps: software::CAPS_SVTAV1,
            build_args: software::build_svtav1,
        }),
        "libaom-av1" => Some(Backend {
            name: "libaom-av1",
            caps: software::CAPS_AOM_AV1,
            build_args: software::build_aom_av1,
        }),
        _ => None,
    }
}

/// All known encoder names in the order the auto-selector prefers them.
///
/// Vendor-aware re-ordering is applied on top of this in `hw_probe`.
pub const KNOWN_ENCODERS: &[&str] = &[
    // AV1-HW first (tiniest bitrate, newest hardware)
    "av1_amf",
    "av1_nvenc",
    "av1_qsv",
    // HEVC-HW
    "hevc_amf",
    "hevc_nvenc",
    "hevc_qsv",
    "hevc_mf",
    // H.264-HW (most compatible)
    "h264_amf",
    "h264_nvenc",
    "h264_qsv",
    "h264_mf",
    // Software AV1
    "libsvtav1",
    "libaom-av1",
    // Software HEVC / H.264 (last resort)
    "libx265",
    "libx264",
];

/// True when the chosen backend is one of the hardware encoders that
/// historically rejects the global `-color_*` flags.  Used by `mod.rs`
/// to decide whether to emit colour-space tags.
pub fn is_hw_encoder(name: &str) -> bool {
    matches!(
        name,
        "h264_amf" | "hevc_amf" | "av1_amf"
            | "h264_nvenc" | "hevc_nvenc" | "av1_nvenc"
            | "h264_qsv" | "hevc_qsv" | "av1_qsv"
            | "h264_vaapi" | "hevc_vaapi" | "av1_vaapi"
    )
}

/// Helper used by AMF backends to append rate-control arguments.
pub(crate) fn amf_rc(cmd: &mut Command, rc: &RcArgs, qp_flags: &[&str]) {
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args([
            "-rc", "cbr",
            "-b:v", br,
            "-maxrate", br,
            "-bufsize", buf,
        ]);
    } else {
        cmd.args(["-rc", "cqp"]);
        for f in qp_flags {
            cmd.args([*f, rc.quality.as_str()]);
        }
    }
}

/// Helper used by NVENC backends to append rate-control arguments.
pub(crate) fn nvenc_rc(cmd: &mut Command, rc: &RcArgs) {
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args([
            "-rc", "cbr",
            "-b:v", br,
            "-maxrate", br,
            "-bufsize", buf,
        ]);
    } else {
        cmd.args(["-rc", "constqp", "-qp", &rc.quality]);
    }
}

/// True when the encoder name is one of those that require yuv444 to be
/// requested via a profile flag rather than `-pix_fmt`.  Currently
/// informational (the chroma profile is already set per-backend).
#[allow(dead_code)]
pub fn _chroma_needs_special_handling(_chroma: Chroma) -> bool {
    false
}
