//! Software encoders (libx264, libx265, libsvtav1, libaom-av1).

use super::{BackendCaps, HwVendor, RcArgs};
use crate::encoder::{Chroma, EncoderConfig};
use std::process::Command;

pub const CAPS_X264: BackendCaps = BackendCaps {
    vendor: HwVendor::Software,
    supports_yuv444: true,
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_X265: BackendCaps = BackendCaps {
    vendor: HwVendor::Software,
    supports_yuv444: true,
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_SVTAV1: BackendCaps = BackendCaps {
    vendor: HwVendor::Software,
    supports_yuv444: false,
    supports_slices: false,
    min_quality: 0,
    max_quality: 63,
};

pub const CAPS_AOM_AV1: BackendCaps = BackendCaps {
    vendor: HwVendor::Software,
    supports_yuv444: true,
    supports_slices: false,
    min_quality: 0,
    max_quality: 63,
};

pub fn build_x264(cmd: &mut Command, cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "libx264",
        "-preset", "ultrafast",
        "-tune", "zerolatency",
    ]);
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args(["-b:v", br, "-maxrate", br, "-bufsize", buf]);
    } else {
        cmd.args(["-crf", &rc.quality]);
    }
    cmd.args([
        "-profile:v",
        if cfg.chroma == Chroma::Yuv444 { "high444" } else { "baseline" },
    ]);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_x265(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "libx265",
        "-preset", "ultrafast",
        "-tune", "zerolatency",
    ]);
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args(["-b:v", br, "-maxrate", br, "-bufsize", buf]);
    } else {
        cmd.args(["-crf", &rc.quality]);
    }
    if rc.want_slices {
        // x265 expects slice count via `-x265-params slices=N`.
        cmd.args(["-x265-params", &format!("slices={}", rc.slices)]);
    }
}

pub fn build_svtav1(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "libsvtav1",
        "-preset", "12",
        "-svtav1-params", "low-latency=1:tune=0",
    ]);
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args(["-b:v", br, "-maxrate", br, "-bufsize", buf]);
    } else {
        cmd.args(["-crf", &rc.quality]);
    }
}

pub fn build_aom_av1(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "libaom-av1",
        "-cpu-used", "8",
        "-row-mt", "1",
        "-tile-columns", "2",
        "-tile-rows", "1",
        "-usage", "realtime",
    ]);
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args(["-b:v", br, "-maxrate", br, "-bufsize", buf]);
    } else {
        cmd.args(["-crf", &rc.quality, "-b:v", "0"]);
    }
}
