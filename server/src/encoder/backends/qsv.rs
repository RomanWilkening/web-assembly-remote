//! Intel Quick Sync Video (QSV) encoders.
//!
//! QSV uses `-global_quality` for ICQ rate control rather than `-qp`,
//! and `-low_power 1` enables the on-die VDEnc fixed-function path that
//! is what makes QSV competitive with NVENC/AMF for low-latency capture.

use super::{BackendCaps, HwVendor, RcArgs};
use crate::encoder::EncoderConfig;
use std::process::Command;

pub const CAPS_H264: BackendCaps = BackendCaps {
    vendor: HwVendor::Intel,
    supports_yuv444: false,
    supports_slices: true,
    min_quality: 1,
    max_quality: 51,
};

pub const CAPS_HEVC: BackendCaps = BackendCaps {
    vendor: HwVendor::Intel,
    supports_yuv444: false,
    supports_slices: true,
    min_quality: 1,
    max_quality: 51,
};

pub const CAPS_AV1: BackendCaps = BackendCaps {
    vendor: HwVendor::Intel,
    supports_yuv444: false,
    supports_slices: false,
    min_quality: 1,
    max_quality: 63,
};

fn qsv_rc(cmd: &mut Command, rc: &RcArgs) {
    if let (Some(br), Some(buf)) = (&rc.bitrate_arg, &rc.vbv_buf_arg) {
        cmd.args([
            "-b:v", br,
            "-maxrate", br,
            "-bufsize", buf,
        ]);
    } else {
        // ICQ: Intelligent Constant Quality.  Equivalent of CRF.
        cmd.args(["-global_quality", &rc.quality]);
    }
}

pub fn build_h264(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "h264_qsv",
        "-preset", "veryfast",
        "-low_power", "1",
        "-async_depth", "1",
    ]);
    qsv_rc(cmd, rc);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_hevc(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "hevc_qsv",
        "-preset", "veryfast",
        "-low_power", "1",
        "-async_depth", "1",
    ]);
    qsv_rc(cmd, rc);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_av1(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "av1_qsv",
        "-preset", "veryfast",
        "-low_power", "1",
        "-async_depth", "1",
    ]);
    qsv_rc(cmd, rc);
}
