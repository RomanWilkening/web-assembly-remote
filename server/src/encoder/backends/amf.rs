//! AMD Advanced Media Framework (AMF) encoders.
//!
//! Quirks worth knowing:
//!
//! * `-forced_idr 1` is **mandatory** — without it `-g` and
//!   `-force_key_frames` produce non-IDR I-slices that the client decoder
//!   cannot use to resync after a dropped delta frame.
//! * `hevc_amf` historically does *not* echo VPS into the bitstream
//!   even with `-header_insertion_mode idr`; the BSF chain in
//!   `mod.rs` (`dump_extra=freq=keyframe,hevc_metadata=aud=insert`)
//!   compensates by prepending `extradata` to every IDR.
//! * AMF does not honour the global `-color_*` flags — `mod.rs` skips
//!   them for AMF builds.

use super::{amf_rc, BackendCaps, HwVendor, RcArgs};
use crate::encoder::{Chroma, EncoderConfig};
use std::process::Command;

pub const CAPS_H264: BackendCaps = BackendCaps {
    vendor: HwVendor::Amd,
    supports_yuv444: true,   // AMD H.264 supports High 4:4:4 profile
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_HEVC: BackendCaps = BackendCaps {
    vendor: HwVendor::Amd,
    supports_yuv444: true,   // RExt profile
    supports_slices: true,
    min_quality: 0,
    max_quality: 51,
};

pub const CAPS_AV1: BackendCaps = BackendCaps {
    vendor: HwVendor::Amd,
    supports_yuv444: false,  // AV1 AMF currently 4:2:0 only
    supports_slices: false,  // AV1 ignores the slices flag
    min_quality: 0,
    max_quality: 63,
};

pub fn build_h264(cmd: &mut Command, cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "h264_amf",
        "-usage", "ultralowlatency",
        "-quality", "speed",
        // h264_amf silently ignores `-g` and the universal
        // `-force_key_frames` expression unless `-forced_idr` is
        // explicitly enabled — without it, requested key-frames are at
        // most non-IDR I-slices and the client decoder cannot use them
        // to resync.
        "-forced_idr", "1",
    ]);
    amf_rc(cmd, rc, &["-qp_i", "-qp_p"]);
    cmd.args([
        "-profile:v",
        if cfg.chroma == Chroma::Yuv444 { "high" } else { "main" },
    ]);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_hevc(cmd: &mut Command, cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "hevc_amf",
        "-usage", "ultralowlatency",
        "-quality", "speed",
        "-forced_idr", "1",
        // Repeat VPS/SPS/PPS in-band before every IDR.  See module-level
        // doc-comment for why this is required for the BSF chain.
        "-header_insertion_mode", "idr",
    ]);
    amf_rc(cmd, rc, &["-qp_i", "-qp_p"]);
    cmd.args([
        "-profile:v",
        if cfg.chroma == Chroma::Yuv444 { "rext" } else { "main" },
    ]);
    if rc.want_slices {
        cmd.args(["-slices", &rc.slices]);
    }
}

pub fn build_av1(cmd: &mut Command, _cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args([
        "-c:v", "av1_amf",
        "-usage", "ultralowlatency",
        "-quality", "speed",
        "-forced_idr", "1",
    ]);
    amf_rc(cmd, rc, &["-qp_i", "-qp_p"]);
}
