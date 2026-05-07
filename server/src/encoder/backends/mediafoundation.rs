//! Microsoft Media Foundation encoders (`h264_mf`, `hevc_mf`).
//!
//! Media Foundation is the Windows-native encode API; on systems where
//! AMF/NVENC/QSV are unavailable but a vendor-specific MFT is installed
//! (Intel iGPUs without QSV, Microsoft Basic Display, certain virtual
//! GPUs) Media Foundation is often the only HW path that works.

use super::{BackendCaps, HwVendor, RcArgs};
use crate::encoder::EncoderConfig;
use std::process::Command;

pub const CAPS_H264: BackendCaps = BackendCaps {
    vendor: HwVendor::Microsoft,
    supports_yuv444: false,
    supports_slices: false,
    min_quality: 0,
    max_quality: 100,
};

pub const CAPS_HEVC: BackendCaps = BackendCaps {
    vendor: HwVendor::Microsoft,
    supports_yuv444: false,
    supports_slices: false,
    min_quality: 0,
    max_quality: 100,
};

fn mf_rc(cmd: &mut Command, rc: &RcArgs) {
    if let Some(br) = &rc.bitrate_arg {
        cmd.args([
            "-rate_control", "cbr",
            "-b:v", br,
        ]);
    } else {
        // Media Foundation expresses quality on a 0..100 scale.  When
        // operators pass a libx264-style 0..51 quality value through to
        // an MF encoder it is interpreted as a (very low) MF quality;
        // validation in `validate.rs` warns when that mismatch happens.
        cmd.args(["-rate_control", "quality", "-quality", &rc.quality]);
    }
}

pub fn build_h264(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "h264_mf",
        "-scenario", "live_streaming",
        "-hw_encoding", "true",
    ]);
    mf_rc(cmd, rc);
}

pub fn build_hevc(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "hevc_mf",
        "-scenario", "live_streaming",
        "-hw_encoding", "true",
    ]);
    mf_rc(cmd, rc);
}
