//! NVIDIA NVENC encoders (h264_nvenc / hevc_nvenc / av1_nvenc).

use super::{nvenc_rc, BackendCaps, HwVendor, RcArgs};
use crate::encoder::EncoderConfig;
use std::process::Command;

pub const CAPS_H264: BackendCaps = BackendCaps {
    vendor: HwVendor::Nvidia,
    supports_yuv444: true,   // h264_nvenc High 4:4:4 Predictive
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_HEVC: BackendCaps = BackendCaps {
    vendor: HwVendor::Nvidia,
    supports_yuv444: true,
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_AV1: BackendCaps = BackendCaps {
    vendor: HwVendor::Nvidia,
    supports_yuv444: false,
    supports_slices: false,
    min_quality: 0,
    max_quality: 63,
};

pub fn build_h264(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "h264_nvenc",
        "-preset", "p1",
        "-tune", "ull",
        "-zerolatency", "1",
    ]);
    nvenc_rc(cmd, rc);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_hevc(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "hevc_nvenc",
        "-preset", "p1",
        "-tune", "ull",
        "-zerolatency", "1",
    ]);
    nvenc_rc(cmd, rc);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_av1(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "av1_nvenc",
        "-preset", "p1",
        "-tune", "ull",
        "-zerolatency", "1",
    ]);
    nvenc_rc(cmd, rc);
}
