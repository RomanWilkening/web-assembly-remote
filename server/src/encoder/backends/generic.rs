//! Generic fallback for encoders we don't have a tailored builder for.
//!
//! Just sets `-c:v <name>` and an optional bitrate; the user is
//! responsible for ensuring the FFmpeg build supports the encoder and
//! for any extra flags it requires.

use super::RcArgs;
use crate::encoder::EncoderConfig;
use std::process::Command;

pub fn build(cmd: &mut Command, cfg: &EncoderConfig, rc: &RcArgs) {
    cmd.args(["-c:v", &cfg.encoder_name]);
    if let Some(br) = &rc.bitrate_arg {
        cmd.args(["-b:v", br]);
    }
}
