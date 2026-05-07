//! FFmpeg-based video encoder.
//!
//! This module replaces the original monolithic `encoder.rs`.  Public
//! types (`CodecKind`, `Chroma`, `EncoderConfig`, `EncodedFrame`,
//! `FfmpegEncoder`) are unchanged so existing call-sites
//! (`server.rs`, `main.rs`) continue to compile without edits.
//!
//! Layout:
//!
//! * [`splitter`]        – codec-specific access-unit splitters
//!                         (H.264, HEVC, AV1).  Trait + impls.
//! * [`backends`]        – per-encoder `BackendCaps` + `build_args`
//!                         (AMF, NVENC, QSV, MediaFoundation,
//!                         software).
//! * [`validate`]        – pre-flight `EncoderConfig` validation.
//! * `FfmpegEncoder`     – manages the FFmpeg subprocess + reader/
//!                         writer threads.
//!
//! The encoder-reader loop also implements an *output-stall watchdog*
//! that publishes diagnostic counters via [`EncoderStats`] so the
//! server can decide to restart or fall back to a different backend
//! when FFmpeg silently stops producing bytes (a known AMF
//! reconfigure-stall failure mode).

pub mod backends;
pub mod splitter;
pub mod validate;

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use self::backends::RcArgs;
pub use self::splitter::{Av1Splitter, FrameSplitter, H264Splitter, HevcSplitter};

/// Video codec used by the encoder. Selects both the FFmpeg codec/format
/// arguments and the corresponding access-unit splitter on the read side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    /// H.264 / AVC — Annex-B byte-stream split on AUD NAL units (type 9).
    H264,
    /// H.265 / HEVC — Annex-B byte-stream split on AUD NAL units (type 35).
    Hevc,
    /// AOMedia Video 1 — Low-Overhead Bitstream Format split on
    /// Temporal Delimiter OBUs (type 2).
    Av1,
}

impl CodecKind {
    /// Auto-detect the codec from an FFmpeg encoder name.
    pub fn from_encoder_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.contains("av1") || n.contains("svtav1") || n.contains("aom") {
            Self::Av1
        } else if n.contains("hevc") || n.contains("h265") || n.contains("265") {
            Self::Hevc
        } else {
            Self::H264
        }
    }

    /// Wire-protocol byte sent to the client in `ServerInfo`.
    pub fn protocol_id(self) -> u8 {
        match self {
            Self::H264 => 0,
            Self::Hevc => 1,
            Self::Av1 => 2,
        }
    }
}

/// Sub-sampling for the chroma planes given to FFmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:2:0 (default) — universally supported in browser hardware decoders.
    Yuv420,
    /// 4:4:4 — sharper text/UI rendering at the cost of larger bitstreams
    /// and reduced HW-decoder availability.
    Yuv444,
}

/// Configuration for an [`FfmpegEncoder`] instance.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality: u8,
    pub encoder_name: String,
    pub codec: CodecKind,
    pub chroma: Chroma,
    pub slices: u32,
    pub bitrate_kbps: Option<u32>,
}

/// One encoded video frame ready to send to the client.
///
/// The first 10 bytes of `data` are reserved for the `MSG_VIDEO_FRAME`
/// wire header so the WebSocket sender can fill it in in-place.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

impl EncodedFrame {
    pub const HEADER_LEN: usize = 10;
}

/// Live diagnostic counters published by an [`FfmpegEncoder`] instance.
///
/// Concurrently writeable from the encoder threads and readable from
/// any other thread (used by the future watchdog and the `/api/stats`
/// HTTP endpoint).
#[derive(Debug, Default)]
pub struct EncoderStats {
    pub bytes_read: AtomicU64,
    pub frames_out: AtomicU64,
    pub keys_out: AtomicU64,
    /// Server-clock microseconds of the most recent byte read from
    /// FFmpeg's stdout.  `0` when no bytes have been read yet.
    pub last_read_us: AtomicU64,
}

impl EncoderStats {
    pub fn snapshot(&self) -> EncoderStatsSnapshot {
        EncoderStatsSnapshot {
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            frames_out: self.frames_out.load(Ordering::Relaxed),
            keys_out: self.keys_out.load(Ordering::Relaxed),
            last_read_us: self.last_read_us.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderStatsSnapshot {
    pub bytes_read: u64,
    pub frames_out: u64,
    pub keys_out: u64,
    pub last_read_us: u64,
}

/// Manages an FFmpeg subprocess that accepts raw BGRA frames on stdin
/// and produces a codec-specific Annex-B / OBU byte-stream on stdout.
pub struct FfmpegEncoder {
    #[allow(dead_code)]
    process: Child,
    writer_tx: std_mpsc::SyncSender<Option<Vec<u8>>>,
    writer_scratch: Vec<u8>,
    stats: Arc<EncoderStats>,
}

impl FfmpegEncoder {
    /// Build the FFmpeg command line for `cfg`.  Public so it can be
    /// unit-tested without spawning FFmpeg.
    pub fn build_command(cfg: &EncoderConfig) -> Command {
        let size = format!("{}x{}", cfg.width, cfg.height);
        let mut cmd = Command::new("ffmpeg");

        // ── input ──────────────────────────────────────────────
        cmd.args([
            "-hide_banner",
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pix_fmt", "bgra",
            "-video_size", &size,
            "-framerate", &cfg.fps.to_string(),
            "-i", "pipe:0",
        ]);

        // ── colour-space tags ──────────────────────────────────
        // Only emitted for software encoders — see the original
        // `encoder.rs` for the rationale.
        if !backends::is_hw_encoder(&cfg.encoder_name) {
            cmd.args([
                "-color_range", "pc",
                "-color_primaries", "bt709",
                "-color_trc", "bt709",
                "-colorspace", "bt709",
            ]);
        }

        // ── chroma sub-sampling ────────────────────────────────
        if cfg.chroma == Chroma::Yuv444 {
            cmd.args(["-pix_fmt", "yuv444p"]);
        }

        // ── encoder-specific flags via the backend table ───────
        let rc = RcArgs::from_config(cfg);
        match backends::lookup(&cfg.encoder_name) {
            Some(b) => (b.build_args)(&mut cmd, cfg, &rc),
            None => backends::generic::build(&mut cmd, cfg, &rc),
        }

        // ── common output flags ────────────────────────────────
        let gop = cfg.fps.to_string();
        cmd.args([
            "-bf", "0",
            "-g", &gop,
            "-force_key_frames", "expr:gte(t,n_forced*1)",
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            "-flush_packets", "1",
        ]);

        // Codec-specific access-unit framing on the output side.
        match cfg.codec {
            CodecKind::H264 => cmd.args([
                "-bsf:v", "h264_metadata=aud=insert",
                "-f", "h264",
                "pipe:1",
            ]),
            CodecKind::Hevc => cmd.args([
                "-bsf:v", "dump_extra=freq=keyframe,hevc_metadata=aud=insert",
                "-f", "hevc",
                "pipe:1",
            ]),
            CodecKind::Av1 => cmd.args([
                "-f", "obu",
                "pipe:1",
            ]),
        };

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd
    }

    /// Spawn FFmpeg and start the background reader/writer threads.
    pub fn new(
        cfg: EncoderConfig,
        frame_tx: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Pre-flight validation for an actionable error message.
        validate::validate(&cfg).map_err(|e| {
            log::error!("Encoder pre-flight validation failed: {e}");
            Box::<dyn std::error::Error>::from(e.to_string())
        })?;

        log::info!(
            "Spawning FFmpeg encoder ({}, codec={:?}, chroma={:?}, slices={}, rc={})…",
            cfg.encoder_name,
            cfg.codec,
            cfg.chroma,
            cfg.slices,
            if cfg.bitrate_kbps.is_some() { "CBR" } else { "CQP" },
        );

        let mut cmd = Self::build_command(&cfg);
        let mut process = cmd.spawn().map_err(|e| {
            format!("Failed to start FFmpeg – is it installed and in PATH? ({e})")
        })?;

        let stdin = BufWriter::new(process.stdin.take().expect("stdin must be piped"));
        let stdout = process.stdout.take().expect("stdout must be piped");
        let stderr = process.stderr.take().expect("stderr must be piped");

        // Background thread: log FFmpeg stderr.
        std::thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) if !l.is_empty() => {
                            log::warn!("FFmpeg: {l}");
                            if let Some(hint) = ffmpeg_stderr_hint(&l) {
                                log::error!("Hint: {hint}");
                            }
                        }
                        Err(e) => {
                            log::debug!("FFmpeg stderr read error: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            })?;

        // Background reader thread.
        let stats = Arc::new(EncoderStats::default());
        let reader_stats = Arc::clone(&stats);
        let codec = cfg.codec;
        std::thread::Builder::new()
            .name("encoder-reader".into())
            .spawn(move || {
                let splitter: Box<dyn FrameSplitter> = match codec {
                    CodecKind::H264 => Box::new(H264Splitter::new()),
                    CodecKind::Hevc => Box::new(HevcSplitter::new()),
                    CodecKind::Av1 => Box::new(Av1Splitter::new()),
                };
                encoder_reader_loop(stdout, splitter, frame_tx, reader_stats);
            })?;

        // Background writer thread.
        let (writer_tx, writer_rx) = std_mpsc::sync_channel::<Option<Vec<u8>>>(1);
        std::thread::Builder::new()
            .name("ffmpeg-stdin".into())
            .spawn(move || {
                encoder_writer_loop(stdin, writer_rx);
            })?;

        Ok(Self {
            process,
            writer_tx,
            writer_scratch: Vec::new(),
            stats,
        })
    }

    /// Hand one raw BGRA frame off to the encoder writer thread.
    /// Newest-wins: when the writer is busy the previously-queued
    /// frame is dropped to keep capture latency at the source rate.
    pub fn send_frame(&mut self, bgra: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.writer_scratch.clear();
        self.writer_scratch.reserve(bgra.len());
        self.writer_scratch.extend_from_slice(bgra);
        let buf = std::mem::take(&mut self.writer_scratch);

        match self.writer_tx.try_send(Some(buf)) {
            Ok(()) => Ok(()),
            Err(std_mpsc::TrySendError::Full(Some(buf))) => {
                log::trace!("encoder busy – dropping frame");
                self.writer_scratch = buf;
                self.writer_scratch.clear();
                Ok(())
            }
            Err(std_mpsc::TrySendError::Full(None)) => Ok(()),
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                Err("FFmpeg writer thread terminated".into())
            }
        }
    }

    /// Live diagnostic counters; safe to call from any thread.
    pub fn stats(&self) -> Arc<EncoderStats> {
        Arc::clone(&self.stats)
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        let _ = self.writer_tx.send(None);
    }
}

fn encoder_writer_loop(
    mut stdin: BufWriter<std::process::ChildStdin>,
    rx: std_mpsc::Receiver<Option<Vec<u8>>>,
) {
    while let Ok(msg) = rx.recv() {
        let buf = match msg {
            Some(b) => b,
            None => {
                log::debug!("encoder writer received shutdown");
                break;
            }
        };
        if let Err(e) = stdin.write_all(&buf) {
            log::error!("FFmpeg stdin write failed: {e}");
            break;
        }
        if let Err(e) = stdin.flush() {
            log::error!("FFmpeg stdin flush failed: {e}");
            break;
        }
    }
}

// ── FFmpeg stderr → actionable hint ────────────────────────────────

pub(crate) fn ffmpeg_stderr_hint(line: &str) -> Option<&'static str> {
    if line.contains("CreateComponent(AMFVideoEncoderHW_AV1) failed") {
        return Some(
            "av1_amf is not supported by this GPU/driver (AMD requires RDNA3 / RX 7000+). \
             Switch to --encoder hevc_amf, --encoder h264_amf, or a software AV1 \
             encoder such as --encoder libsvtav1.",
        );
    }
    if line.contains("OpenEncodeSessionEx failed: unsupported device")
        || line.contains("Cannot load nvEncodeAPI")
    {
        return Some(
            "NVENC is not available (no NVIDIA GPU, missing driver, or AV1 NVENC \
             needs RTX 4000+).  Switch to a different --encoder.",
        );
    }
    if line.contains("hevc_metadata") && line.contains("VPS id 0 not available") {
        return Some(
            "hevc_amf is emitting an HEVC bitstream without VPS in-band and the \
             encoder's extradata isn't being prepended.  Make sure the BSF chain \
             starts with `dump_extra=freq=keyframe` (the default ships this).",
        );
    }
    None
}

// ── Encoded byte-stream reader ─────────────────────────────────────

fn encoder_reader_loop(
    mut stdout: impl Read,
    mut splitter: Box<dyn FrameSplitter>,
    tx: mpsc::Sender<EncodedFrame>,
    stats: Arc<EncoderStats>,
) {
    let mut buf = vec![0u8; 128 * 1024];
    let mut frames: Vec<EncodedFrame> = Vec::with_capacity(2);

    let mut last_report = Instant::now();
    let report_every = Duration::from_secs(5);
    let pipeline_start = Instant::now();

    loop {
        match stdout.read(&mut buf) {
            Ok(0) => {
                let s = stats.snapshot();
                log::info!(
                    "FFmpeg stdout closed (bytes_read={}, frames_out={}, keys_out={})",
                    s.bytes_read, s.frames_out, s.keys_out,
                );
                break;
            }
            Ok(n) => {
                stats.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
                let now_us = pipeline_start.elapsed().as_micros() as u64;
                stats.last_read_us.store(now_us, Ordering::Relaxed);

                frames.clear();
                splitter.push(&buf[..n], &mut frames);
                for frame in frames.drain(..) {
                    let frames_out = stats.frames_out.fetch_add(1, Ordering::Relaxed) + 1;
                    if frame.is_keyframe {
                        let keys_out = stats.keys_out.fetch_add(1, Ordering::Relaxed) + 1;
                        if frames_out > 2 {
                            log::info!(
                                "encoder-reader: emitted KEY frame #{} (#{} key, {} bytes payload)",
                                frames_out, keys_out,
                                frame.data.len() - EncodedFrame::HEADER_LEN,
                            );
                        }
                    }
                    if frames_out <= 2 {
                        log::info!(
                            "encoder-reader: emitted frame #{} ({} bytes payload, key={})",
                            frames_out,
                            frame.data.len() - EncodedFrame::HEADER_LEN,
                            frame.is_keyframe,
                        );
                    }
                    if tx.blocking_send(frame).is_err() {
                        let s = stats.snapshot();
                        log::info!(
                            "Frame channel closed – stopping reader (bytes_read={}, frames_out={}, keys_out={})",
                            s.bytes_read, s.frames_out, s.keys_out,
                        );
                        return;
                    }
                }
                if last_report.elapsed() >= report_every {
                    let s = stats.snapshot();
                    log::info!(
                        "encoder-reader: bytes_read={}, frames_out={}, keys_out={}, splitter_buf={}",
                        s.bytes_read, s.frames_out, s.keys_out,
                        splitter.buffered_bytes(),
                    );
                    last_report = Instant::now();
                }
            }
            Err(e) => {
                let s = stats.snapshot();
                log::error!(
                    "FFmpeg read error: {e} (bytes_read={}, frames_out={}, keys_out={})",
                    s.bytes_read, s.frames_out, s.keys_out,
                );
                break;
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    fn cmd_args(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn codec_kind_auto_detection() {
        assert_eq!(CodecKind::from_encoder_name("h264_amf"), CodecKind::H264);
        assert_eq!(CodecKind::from_encoder_name("libx264"), CodecKind::H264);
        assert_eq!(CodecKind::from_encoder_name("hevc_amf"), CodecKind::Hevc);
        assert_eq!(CodecKind::from_encoder_name("h265_nvenc"), CodecKind::Hevc);
        assert_eq!(CodecKind::from_encoder_name("libx265"), CodecKind::Hevc);
        assert_eq!(CodecKind::from_encoder_name("av1_amf"), CodecKind::Av1);
        assert_eq!(CodecKind::from_encoder_name("libsvtav1"), CodecKind::Av1);
        assert_eq!(CodecKind::from_encoder_name("libaom-av1"), CodecKind::Av1);
        assert_eq!(CodecKind::from_encoder_name("unknown"), CodecKind::H264);
    }

    #[test]
    fn codec_kind_protocol_id_stable() {
        assert_eq!(CodecKind::H264.protocol_id(), 0);
        assert_eq!(CodecKind::Hevc.protocol_id(), 1);
        assert_eq!(CodecKind::Av1.protocol_id(), 2);
    }

    #[test]
    fn ffmpeg_stderr_hint_detects_av1_amf_unsupported() {
        let line = "[av1_amf @ 000002b2687ee4c0] CreateComponent(AMFVideoEncoderHW_AV1) \
                    failed with error 30";
        let hint = ffmpeg_stderr_hint(line).expect("must produce a hint");
        assert!(hint.contains("RDNA3"));
        assert!(hint.contains("--encoder"));
    }

    #[test]
    fn ffmpeg_stderr_hint_detects_hevc_metadata_missing_vps() {
        let line = "[hevc_metadata @ 000001cb4bc0c040] VPS id 0 not available.";
        assert!(ffmpeg_stderr_hint(line).is_some());
    }

    #[test]
    fn ffmpeg_stderr_hint_ignores_normal_progress_lines() {
        for line in [
            "frame=  120 fps= 60 q=20.0 size=     128KiB time=00:00:02.00",
            "Stream #0:0: Video: hevc_amf, 2560x1440, 60 fps",
            "",
        ] {
            assert!(
                ffmpeg_stderr_hint(line).is_none(),
                "must not flag noise: {line:?}",
            );
        }
    }

    #[test]
    fn build_command_h264_amf_has_forced_idr_and_high_profile_for_yuv444() {
        let mut c = cfg("h264_amf");
        c.chroma = Chroma::Yuv444;
        let cmd = FfmpegEncoder::build_command(&c);
        let args = cmd_args(&cmd);
        assert!(args.iter().any(|a| a == "h264_amf"));
        assert!(args.windows(2).any(|w| w == ["-forced_idr", "1"]));
        assert!(args.windows(2).any(|w| w == ["-profile:v", "high"]));
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv444p"]));
    }

    #[test]
    fn build_command_libx264_has_color_tags_but_h264_amf_does_not() {
        let cmd_sw = FfmpegEncoder::build_command(&cfg("libx264"));
        let args_sw = cmd_args(&cmd_sw);
        assert!(args_sw.windows(2).any(|w| w == ["-colorspace", "bt709"]));

        let cmd_hw = FfmpegEncoder::build_command(&cfg("h264_amf"));
        let args_hw = cmd_args(&cmd_hw);
        assert!(!args_hw.windows(2).any(|w| w == ["-colorspace", "bt709"]));
    }

    #[test]
    fn build_command_hevc_uses_dump_extra_bsf_chain() {
        let cmd = FfmpegEncoder::build_command(&cfg("hevc_amf"));
        let args = cmd_args(&cmd);
        let bsf = args
            .windows(2)
            .find(|w| w[0] == "-bsf:v")
            .map(|w| w[1].clone())
            .expect("must set -bsf:v");
        assert!(bsf.starts_with("dump_extra=freq=keyframe"));
        assert!(bsf.contains("hevc_metadata=aud=insert"));
    }

    #[test]
    fn build_command_av1_uses_obu_container() {
        let cmd = FfmpegEncoder::build_command(&cfg("av1_amf"));
        let args = cmd_args(&cmd);
        assert!(args.windows(2).any(|w| w == ["-f", "obu"]));
    }

    #[test]
    fn build_command_bitrate_overrides_quality_for_amf() {
        let mut c = cfg("h264_amf");
        c.bitrate_kbps = Some(8000);
        let args = cmd_args(&FfmpegEncoder::build_command(&c));
        assert!(args.windows(2).any(|w| w == ["-rc", "cbr"]));
        assert!(args.windows(2).any(|w| w == ["-b:v", "8000k"]));
        assert!(args.windows(2).any(|w| w == ["-maxrate", "8000k"]));
    }

    #[test]
    fn build_command_quality_mode_uses_qp_for_amf() {
        let cmd = FfmpegEncoder::build_command(&cfg("h264_amf"));
        let args = cmd_args(&cmd);
        assert!(args.windows(2).any(|w| w == ["-rc", "cqp"]));
        assert!(args.windows(2).any(|w| w == ["-qp_i", "20"]));
        assert!(args.windows(2).any(|w| w == ["-qp_p", "20"]));
    }
}
