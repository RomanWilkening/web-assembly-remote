use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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
    /// Auto-detect the codec from an FFmpeg encoder name (e.g. `h264_amf`,
    /// `hevc_nvenc`, `libx265`, `libsvtav1`). Falls back to H.264 for
    /// anything we don't recognise so behaviour stays compatible with
    /// existing configurations.
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

    /// Wire-protocol byte sent to the client in `ServerInfo` so the
    /// browser can configure the matching `VideoDecoder`.
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

/// Configuration for an [`FfmpegEncoder`] instance.  Bundled into a
/// struct so adding new options in future does not break call-sites.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Constant-quality value (QP for AMF/H.264, CRF for libx264/libx265,
    /// CRF for libsvtav1).  Only used when `bitrate_kbps` is `None`.
    pub quality: u8,
    /// FFmpeg encoder name, e.g. `h264_amf` / `hevc_amf` / `av1_amf` /
    /// `libx264` / `libx265` / `libsvtav1`.
    pub encoder_name: String,
    /// Codec family used by the access-unit splitter. Independent from
    /// `encoder_name` so callers can override the auto-detection.
    pub codec: CodecKind,
    /// Chroma sub-sampling.
    pub chroma: Chroma,
    /// Number of slices per encoded frame (>= 1).  Slicing reduces the
    /// "wait for the whole frame to arrive before decoding" latency:
    /// each slice can in principle be sent and decoded independently.
    /// Anything > 1 only takes effect for codecs / encoders that honour
    /// the `-slices N` flag (H.264/HEVC).  AV1 ignores it.
    pub slices: u32,
    /// If set, switches rate control from constant-quality (CQP/CRF) to
    /// constant-bitrate with a 1-frame VBV buffer for minimum latency.
    /// Useful on links where bandwidth is the bottleneck and visible
    /// quality variation is preferable to glass-to-glass lag spikes.
    pub bitrate_kbps: Option<u32>,
}

/// One encoded video frame ready to send to the client.
///
/// `data` is laid out so that the first 10 bytes are reserved for the
/// `MSG_VIDEO_FRAME` wire header (`type u8 + timestamp_us u64 LE +
/// is_keyframe u8`) and the remainder is the raw codec access-unit
/// payload (H.264 / HEVC Annex-B, or a sequence of AV1 OBUs ending in a
/// Temporal Delimiter).  This lets the WebSocket sender fill in the
/// header in-place and forward the whole `Vec` to
/// `axum::Message::Binary` without any further copy of the (potentially
/// multi-MB) compressed payload.
pub struct EncodedFrame {
    /// `[0..10]` reserved header, `[10..]` codec payload.
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

impl EncodedFrame {
    /// Length of the reserved header region.
    pub const HEADER_LEN: usize = 10;
}

/// Manages an FFmpeg subprocess that accepts raw BGRA frames on stdin
/// and produces an H.264 Annex-B byte-stream on stdout.
///
/// Frame writes are decoupled from the capture thread by a dedicated
/// writer OS thread: `send_frame` only hands a `Vec<u8>` to a
/// 1-slot synchronous channel and never blocks on the (potentially
/// multi-millisecond) write/flush into FFmpeg's stdin.  When the
/// encoder is busy and the slot is occupied the *previous* pending
/// frame is dropped — so capture can keep collecting fresh DXGI frames
/// at the source FPS instead of stalling behind a slow encoder.
pub struct FfmpegEncoder {
    #[allow(dead_code)]
    process: Child,
    /// 1-slot bounded channel to the writer thread.  `Some(buf)` means
    /// "encode this frame next"; an attempt to send into an already
    /// occupied slot replaces the queued frame (newest wins, oldest
    /// dropped).  `None` signals the writer to exit.
    writer_tx: std_mpsc::SyncSender<Option<Vec<u8>>>,
    /// Reusable scratch buffer that `send_frame` clones the BGRA slice
    /// into before pushing to the writer channel.  Kept here so we can
    /// recycle it across calls instead of allocating a fresh `Vec`
    /// every frame.  Actual ownership transfer to the channel still
    /// requires a `mem::take`.
    writer_scratch: Vec<u8>,
}

impl FfmpegEncoder {
    /// Spawn FFmpeg and start the background reader thread that
    /// parses access-unit boundaries and pushes complete frames into
    /// `frame_tx`.
    pub fn new(
        cfg: EncoderConfig,
        frame_tx: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let size = format!("{}x{}", cfg.width, cfg.height);

        let mut cmd = Command::new("ffmpeg");

        // ── input ──────────────────────────────────────────────
        cmd.args([
            "-hide_banner",
            "-loglevel", "error",
            // Raw BGRA frames on stdin
            "-f", "rawvideo",
            "-pix_fmt", "bgra",
            "-video_size", &size,
            "-framerate", &cfg.fps.to_string(),
            "-i", "pipe:0",
        ]);

        // ── colour-space tags ──────────────────────────────────
        // The desktop is sRGB / full-range; without explicit tags the
        // browser decoder assumes BT.601 limited range and renders
        // colours slightly desaturated.  BT.709 + PC range matches what
        // Sunshine and Moonlight emit.
        //
        // We only emit these flags for software encoders.  AMD AMF and
        // NVENC builds historically reject or mishandle the global
        // `-color_*` arguments — on some FFmpeg/AMF combinations they
        // trigger an internal reconfigure after the first frame and the
        // pipeline stalls — so we let those encoders write their own
        // SPS VUI fields (which both vendors already do correctly for
        // 8-bit RGB capture) instead.
        let is_hw_encoder = matches!(
            cfg.encoder_name.as_str(),
            "h264_amf" | "hevc_amf" | "av1_amf"
                | "h264_nvenc" | "hevc_nvenc" | "av1_nvenc"
                | "h264_qsv" | "hevc_qsv" | "av1_qsv"
                | "h264_vaapi" | "hevc_vaapi" | "av1_vaapi"
        );
        if !is_hw_encoder {
            cmd.args([
                "-color_range", "pc",
                "-color_primaries", "bt709",
                "-color_trc", "bt709",
                "-colorspace", "bt709",
            ]);
        }

        // ── pixel-format hint (chroma sub-sampling) ────────────
        // For the default 4:2:0 case we let FFmpeg auto-negotiate the
        // BGRA → encoder-native conversion (this matches the pre-PR
        // behaviour that the stable-release encoders such as `h264_amf`
        // were tuned against — those want NV12, and forcing an
        // intermediate `-vf format=yuv420p` filter chain combined with
        // `-flags low_delay` could deadlock the encoder after the first
        // emitted packet on some Windows AMF builds).
        //
        // For 4:4:4 we *do* need to force the format because the
        // automatic negotiation defaults to 4:2:0 even when the encoder
        // can ingest 4:4:4 data.
        let pix_fmt = match cfg.chroma {
            Chroma::Yuv420 => None,
            Chroma::Yuv444 => Some("yuv444p"),
        };
        if let Some(pf) = pix_fmt {
            cmd.args(["-pix_fmt", pf]);
        }

        // ── encoder-specific flags ─────────────────────────────
        let q = cfg.quality.to_string();
        let slices_n = cfg.slices.max(1);
        let slices = slices_n.to_string();
        // `-slices` is only added when the user opts in (>1).  Some
        // encoder builds (notably older `h264_amf` releases) do not
        // expose the codec-generic `slices` AVOption and will reject
        // the flag — keeping the default at "no flag" preserves the
        // historical, known-working command line for HW encoders.
        let want_slices = slices_n > 1;
        let bitrate_arg = cfg.bitrate_kbps.map(|kb| format!("{}k", kb));
        let vbv_buf_arg = cfg
            .bitrate_kbps
            .map(|kb| format!("{}", (kb * 1000) / cfg.fps.max(1)));

        // Helper closure: append rate-control args for the AMF family
        // (h264_amf / hevc_amf / av1_amf).  `qp_*` flags are codec
        // dependent so they are passed in by the caller.
        let amf_rc = |cmd: &mut Command, qp_flags: &[&str]| {
            if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                cmd.args([
                    "-rc", "cbr",
                    "-b:v", br,
                    "-maxrate", br,
                    "-bufsize", buf,
                ]);
            } else {
                cmd.args(["-rc", "cqp"]);
                for f in qp_flags {
                    cmd.args([*f, q.as_str()]);
                }
            }
        };

        match cfg.encoder_name.as_str() {
            "h264_amf" => {
                cmd.args([
                    "-c:v", "h264_amf",
                    "-usage", "ultralowlatency",
                    "-quality", "speed",
                    // h264_amf silently ignores `-g` and the universal
                    // `-force_key_frames` expression unless `-forced_idr`
                    // is explicitly enabled — without it, requested
                    // key-frames are at most non-IDR I-slices and the
                    // client decoder cannot use them to resync.  See the
                    // discussion in the common output-flags block where
                    // `-force_key_frames "expr:gte(t,n_forced*1)"` is
                    // configured.
                    "-forced_idr", "1",
                ]);
                amf_rc(&mut cmd, &["-qp_i", "-qp_p"]);
                cmd.args([
                    "-profile:v",
                    if cfg.chroma == Chroma::Yuv444 { "high" } else { "main" },
                ]);
                if want_slices {
                    cmd.args(["-slices", &slices]);
                }
            }
            "hevc_amf" => {
                cmd.args([
                    "-c:v", "hevc_amf",
                    "-usage", "ultralowlatency",
                    "-quality", "speed",
                    // See the comment on h264_amf above — same quirk.
                    "-forced_idr", "1",
                    // Repeat VPS/SPS/PPS in-band before every IDR.
                    //
                    // hevc_amf's default `header_insertion_mode=none`
                    // puts parameter sets only in `extradata`, never
                    // in the bitstream.  Our `-bsf:v
                    // hevc_metadata=aud=insert` post-filter walks the
                    // bitstream looking for VPS/SPS/PPS to compute AUD
                    // placement and aborts the encode with "VPS id 0
                    // not available / Failed to read unit 0 (type 33)"
                    // when none are present (observed on real
                    // RX hardware, see commit message).  Switching to
                    // `idr` makes hevc_amf prepend the parameter-set
                    // bundle to every IDR access unit, which is also
                    // what the WebCodecs HEVC decoder needs to pick up
                    // a stream mid-flight (annexb-with-headers, no
                    // out-of-band hvcC).  Tiny per-IDR overhead
                    // (~80 bytes / 1 s at 60 fps).
                    "-header_insertion_mode", "idr",
                ]);
                amf_rc(&mut cmd, &["-qp_i", "-qp_p"]);
                cmd.args([
                    "-profile:v",
                    if cfg.chroma == Chroma::Yuv444 { "rext" } else { "main" },
                ]);
                if want_slices {
                    cmd.args(["-slices", &slices]);
                }
            }
            "av1_amf" => {
                cmd.args([
                    "-c:v", "av1_amf",
                    "-usage", "ultralowlatency",
                    "-quality", "speed",
                    // See the comment on h264_amf above — same quirk.
                    "-forced_idr", "1",
                ]);
                // av1_amf uses `-qp_i / -qp_p` like the H.264/HEVC AMF
                // encoders.  Slicing flag is not honoured by AV1.
                amf_rc(&mut cmd, &["-qp_i", "-qp_p"]);
            }
            "h264_nvenc" => {
                cmd.args([
                    "-c:v", "h264_nvenc",
                    "-preset", "p1",
                    "-tune", "ull",
                    "-zerolatency", "1",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-rc", "cbr",
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-rc", "constqp", "-qp", &q]);
                }
                if want_slices {
                    cmd.args(["-slices", &slices]);
                }
            }
            "hevc_nvenc" => {
                cmd.args([
                    "-c:v", "hevc_nvenc",
                    "-preset", "p1",
                    "-tune", "ull",
                    "-zerolatency", "1",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-rc", "cbr",
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-rc", "constqp", "-qp", &q]);
                }
                if want_slices {
                    cmd.args(["-slices", &slices]);
                }
            }
            "av1_nvenc" => {
                cmd.args([
                    "-c:v", "av1_nvenc",
                    "-preset", "p1",
                    "-tune", "ull",
                    "-zerolatency", "1",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-rc", "cbr",
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-rc", "constqp", "-qp", &q]);
                }
            }
            "libx264" => {
                cmd.args([
                    "-c:v", "libx264",
                    "-preset", "ultrafast",
                    "-tune", "zerolatency",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-crf", &q]);
                }
                cmd.args([
                    "-profile:v",
                    if cfg.chroma == Chroma::Yuv444 { "high444" } else { "baseline" },
                ]);
                if want_slices {
                    cmd.args(["-slices", &slices]);
                }
            }
            "libx265" => {
                cmd.args([
                    "-c:v", "libx265",
                    "-preset", "ultrafast",
                    "-tune", "zerolatency",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-crf", &q]);
                }
                if want_slices {
                    // x265 expects slice count via `-x265-params slices=N`.
                    cmd.args([
                        "-x265-params",
                        &format!("slices={}", slices),
                    ]);
                }
            }
            "libsvtav1" => {
                cmd.args([
                    "-c:v", "libsvtav1",
                    "-preset", "12",
                    "-svtav1-params", "low-latency=1:tune=0",
                ]);
                if let (Some(br), Some(buf)) = (&bitrate_arg, &vbv_buf_arg) {
                    cmd.args([
                        "-b:v", br,
                        "-maxrate", br,
                        "-bufsize", buf,
                    ]);
                } else {
                    cmd.args(["-crf", &q]);
                }
            }
            other => {
                // Generic: just set the codec; user is responsible for
                // the correct FFmpeg build and any extra flags they need.
                cmd.args(["-c:v", other]);
                if let Some(br) = &bitrate_arg {
                    cmd.args(["-b:v", br]);
                }
            }
        }

        // ── common output flags ────────────────────────────────
        // Key-frame cadence: every 1 second.  Two mechanisms are used
        // belt-and-suspenders:
        //
        //   * `-g {fps}` sets the encoder's internal GOP size so the
        //     rate-control budget is sized for ~1 IDR/s.
        //   * `-force_key_frames` is an FFmpeg-level mechanism that
        //     marks every Nth input frame for IDR encoding regardless
        //     of whether the underlying encoder honours `-g` (some
        //     hardware encoders, notably h264_amf on certain AMD
        //     drivers, ignore `-g` and produce a single IDR at the
        //     start of the stream — leaving the client decoder unable
        //     to recover whenever the ws-sender drops an intermediate
        //     P-frame, since each P-frame references the previous one).
        //     With a 1-second IDR cadence the decoder resyncs at most
        //     ~1 s after any dropped delta and keeps producing frames.
        let gop = cfg.fps.to_string(); // key-frame every 1 second
        cmd.args([
            "-bf", "0",               // no B-frames
            "-g", &gop,
            "-force_key_frames", "expr:gte(t,n_forced*1)",
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            // CRITICAL for low-latency pipe output: by default FFmpeg's
            // muxer (and the underlying libc stdio writer for pipe:1)
            // accumulates 4–8 KiB before flushing.  At 60 fps a static
            // desktop produces H.264 delta frames well below 100 bytes
            // each, which means the first IDR (50–200 KiB) pushes
            // through immediately but subsequent delta frames sit in
            // the buffer for *seconds* — exactly the symptom of "first
            // frame visible, then 'No video frame received for 5000ms'
            // on the client".  `-flush_packets 1` forces the muxer to
            // flush after every packet, so each encoded frame reaches
            // our reader thread as soon as the encoder produces it.
            // Note: `-fflags nobuffer` and `-flags low_delay` above
            // affect the *input* demuxer / decoder reorder queue, not
            // the output writer, so this flag is independently needed.
            "-flush_packets", "1",
        ]);

        // Codec-specific access-unit framing on the output side.  The
        // splitter on the read side (see `make_splitter`) keys off the
        // exact same delimiters.
        match cfg.codec {
            CodecKind::H264 => cmd.args([
                "-bsf:v", "h264_metadata=aud=insert",
                "-f", "h264",
                "pipe:1",
            ]),
            CodecKind::Hevc => cmd.args([
                // Two-stage bitstream filter chain:
                //
                //   1. `dump_extra=freq=keyframe` prepends the encoder's
                //      `extradata` (which always contains VPS+SPS+PPS
                //      for HEVC) to every keyframe packet.  Required
                //      because `hevc_amf` on many AMF runtimes leaves
                //      VPS *only* in extradata even with
                //      `-header_insertion_mode idr` — `header_insertion_mode`
                //      controls SPS/PPS placement but the AMF SDK
                //      historically does not echo VPS into the
                //      bitstream.  Without VPS in-band the next BSF in
                //      the chain (`hevc_metadata`) refuses to start
                //      with "VPS id 0 not available / Failed to read
                //      unit 0 (type 33)".
                //   2. `hevc_metadata=aud=insert` then walks the
                //      now-complete parameter-set-prefixed bitstream
                //      and inserts AUDs (NAL type 35) between access
                //      units.  Our HevcSplitter on the read side keys
                //      off these AUDs.
                //
                // Side benefit: VPS+SPS+PPS in front of every IDR also
                // lets WebCodecs clients joining mid-stream resync at
                // the next 1 s IDR boundary instead of waiting for a
                // separate `extradata` blob.
                "-bsf:v", "dump_extra=freq=keyframe,hevc_metadata=aud=insert",
                "-f", "hevc",
                "pipe:1",
            ]),
            CodecKind::Av1 => cmd.args([
                // The AV1 Low-Overhead Bitstream Format always carries a
                // Temporal Delimiter at every frame boundary, so no
                // additional bitstream filter is needed.
                "-f", "obu",
                "pipe:1",
            ]),
        };

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        log::info!(
            "Spawning FFmpeg encoder ({}, codec={:?}, chroma={:?}, slices={}, rc={})…",
            cfg.encoder_name,
            cfg.codec,
            cfg.chroma,
            cfg.slices,
            if cfg.bitrate_kbps.is_some() { "CBR" } else { "CQP" },
        );

        let mut process = cmd.spawn().map_err(|e| {
            format!("Failed to start FFmpeg – is it installed and in PATH? ({e})")
        })?;

        let stdin = BufWriter::new(
            process.stdin.take().expect("stdin must be piped"),
        );

        let stdout = process.stdout.take().expect("stdout must be piped");
        let stderr = process.stderr.take().expect("stderr must be piped");

        // Background thread: log FFmpeg stderr so encoder errors are visible.
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

        // Background reader thread: reads the encoded byte-stream,
        // splits it into per-frame access units and pushes them into
        // the channel.  The exact splitter is selected by codec.
        let codec = cfg.codec;
        std::thread::Builder::new()
            .name("encoder-reader".into())
            .spawn(move || {
                let splitter: Box<dyn FrameSplitter> = match codec {
                    CodecKind::H264 => Box::new(H264Splitter::new()),
                    CodecKind::Hevc => Box::new(HevcSplitter::new()),
                    CodecKind::Av1 => Box::new(Av1Splitter::new()),
                };
                encoder_reader_loop(stdout, splitter, frame_tx);
            })?;

        // Background writer thread: owns the BufWriter to FFmpeg's
        // stdin so the (potentially multi-ms) write+flush never
        // blocks the capture thread.  A 1-slot sync channel acts as
        // backpressure: if the encoder is still digesting the
        // previous frame, the *new* frame replaces it (capture-side
        // newest-wins), preserving capture FPS at the cost of
        // dropping intermediate frames.
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
        })
    }

    /// Hand one raw BGRA frame off to the encoder writer thread.
    ///
    /// Non-blocking: if the writer is still draining the previous
    /// frame the new frame replaces the queued one (newest-wins
    /// backpressure) so the caller never stalls behind FFmpeg.  The
    /// dropped frame is logged at trace level.
    pub fn send_frame(&mut self, bgra: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        // Reuse our scratch Vec to avoid an allocation per call:
        // clear it, copy in the BGRA bytes, then `take` to move
        // ownership into the channel.
        self.writer_scratch.clear();
        self.writer_scratch.reserve(bgra.len());
        self.writer_scratch.extend_from_slice(bgra);
        let buf = std::mem::take(&mut self.writer_scratch);

        match self.writer_tx.try_send(Some(buf)) {
            Ok(()) => Ok(()),
            Err(std_mpsc::TrySendError::Full(Some(buf))) => {
                // Encoder is busy with the previous frame.  Drop the
                // new one to keep capture latency at the source rate.
                // We *could* alternatively block, but blocking the
                // capture thread is exactly what this whole change is
                // designed to avoid.
                log::trace!("encoder busy – dropping frame");
                // Recycle the buffer instead of letting it drop.
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
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // Best-effort: tell the writer thread to exit so it can flush
        // and close stdin cleanly.  Ignore errors — the worst case is
        // the process is already gone.
        let _ = self.writer_tx.send(None);
    }
}

/// Drain a 1-slot channel of BGRA frame buffers and push each one to
/// FFmpeg's stdin.  Exits cleanly when the channel is closed or `None`
/// is received.
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
    // Drop `stdin` to close the pipe — signals EOF to FFmpeg.
}

// ── FFmpeg stderr → actionable hint ────────────────────────────────

/// Translate a single line of FFmpeg stderr into a one-line operator
/// hint when the message corresponds to a known, non-obvious failure
/// mode that the raw text doesn't fully explain.
///
/// Returns `None` when the line is just normal FFmpeg progress / info
/// noise — we don't want to spam a hint per line.
///
/// Kept as a free function (not a method) so it can be unit-tested
/// without spinning up a real FFmpeg process.
pub(crate) fn ffmpeg_stderr_hint(line: &str) -> Option<&'static str> {
    // av1_amf failing with AMF error 30 (`AMF_NOT_SUPPORTED`) means
    // the GPU/driver does not expose an AV1 hardware encoder.  This is
    // a per-card capability — AMD added AV1 encode in RDNA3 (RX 7000),
    // NVIDIA in Ada Lovelace (RTX 4000), Intel in Arc Alchemist.  Older
    // hardware can't be made to support it, so the only useful action
    // for the operator is to switch encoder.
    if line.contains("CreateComponent(AMFVideoEncoderHW_AV1) failed") {
        return Some(
            "av1_amf is not supported by this GPU/driver (AMD requires RDNA3 / RX 7000+). \
             Switch to --encoder hevc_amf, --encoder h264_amf, or a software AV1 \
             encoder such as --encoder libsvtav1.",
        );
    }

    // Same idea for NVENC.
    if line.contains("OpenEncodeSessionEx failed: unsupported device")
        || line.contains("Cannot load nvEncodeAPI")
    {
        return Some(
            "NVENC is not available (no NVIDIA GPU, missing driver, or AV1 NVENC \
             needs RTX 4000+).  Switch to a different --encoder.",
        );
    }

    // hevc_metadata BSF refusing to start because hevc_amf isn't
    // emitting parameter sets in-band.  Should be fixed by prepending
    // `dump_extra=freq=keyframe` to the BSF chain (we ship this), so
    // if an operator still hits it on a custom build we want them to
    // know which knob to look at.
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

/// Continuously reads from `stdout` and pushes complete access units
/// produced by the codec-specific [`FrameSplitter`] into the channel.
fn encoder_reader_loop(
    mut stdout: impl Read,
    mut splitter: Box<dyn FrameSplitter>,
    tx: mpsc::Sender<EncodedFrame>,
) {
    let mut buf = vec![0u8; 128 * 1024]; // 128 KiB read buffer
    // Reusable output Vec for splitter::push.  Reused across every
    // read so we don't allocate ~60 throwaway Vecs/s on the hot path.
    let mut frames: Vec<EncodedFrame> = Vec::with_capacity(2);

    // ── Diagnostic counters ──────────────────────────────────────────
    // Help diagnose pipeline stalls (e.g. "first frame visible, then
    // nothing"): we emit a one-line summary every 5 seconds so the
    // operator can tell at a glance whether
    //   * FFmpeg is producing bytes (`bytes_read`),
    //   * the splitter is recognising access-unit boundaries
    //     (`frames_out`), and
    //   * the channel-send is succeeding.
    // The first two emissions are also logged unconditionally so a
    // working pipeline shows up in the log without waiting 5 s.
    let mut bytes_read: u64 = 0;
    let mut frames_out: u64 = 0;
    let mut keys_out: u64 = 0;
    let mut last_report = Instant::now();
    let report_every = Duration::from_secs(5);

    loop {
        match stdout.read(&mut buf) {
            Ok(0) => {
                log::info!(
                    "FFmpeg stdout closed (bytes_read={}, frames_out={}, keys_out={})",
                    bytes_read,
                    frames_out,
                    keys_out
                );
                break;
            }
            Ok(n) => {
                bytes_read += n as u64;
                frames.clear();
                splitter.push(&buf[..n], &mut frames);
                for frame in frames.drain(..) {
                    let was = frames_out;
                    frames_out += 1;
                    if frame.is_keyframe {
                        keys_out += 1;
                    }
                    if was < 2 {
                        log::info!(
                            "encoder-reader: emitted frame #{} ({} bytes payload, key={})",
                            frames_out,
                            frame.data.len() - EncodedFrame::HEADER_LEN,
                            frame.is_keyframe
                        );
                    } else if frame.is_keyframe {
                        // Log every keyframe past the initial pair so an
                        // operator can verify the encoder is honouring the
                        // requested IDR cadence (some hardware encoders
                        // ignore `-g` and need `-force_key_frames`).
                        log::info!(
                            "encoder-reader: emitted KEY frame #{} (#{} key, {} bytes payload)",
                            frames_out,
                            keys_out,
                            frame.data.len() - EncodedFrame::HEADER_LEN,
                        );
                    }
                    if tx.blocking_send(frame).is_err() {
                        log::info!(
                            "Frame channel closed – stopping reader (bytes_read={}, frames_out={}, keys_out={})",
                            bytes_read,
                            frames_out,
                            keys_out
                        );
                        return;
                    }
                }
                if last_report.elapsed() >= report_every {
                    log::info!(
                        "encoder-reader: bytes_read={}, frames_out={}, keys_out={}, splitter_buf={}",
                        bytes_read,
                        frames_out,
                        keys_out,
                        splitter.buffered_bytes(),
                    );
                    last_report = Instant::now();
                }
            }
            Err(e) => {
                log::error!(
                    "FFmpeg read error: {e} (bytes_read={}, frames_out={}, keys_out={})",
                    bytes_read,
                    frames_out,
                    keys_out
                );
                break;
            }
        }
    }
}

// ── Codec-specific access-unit splitting ───────────────────────────

/// Codec-agnostic access-unit splitter trait.  Implementations buffer
/// the encoder output stream and return one `EncodedFrame` per complete
/// access unit (one per displayed picture).
pub trait FrameSplitter: Send {
    /// Append `data` to the internal buffer and append any complete
    /// access units detected to `out`.  Implementations must reserve
    /// `EncodedFrame::HEADER_LEN` bytes at the start of each emitted
    /// frame's `data` so the WebSocket sender can fill in the wire
    /// header in place.
    ///
    /// `out` is owned by the caller so the same `Vec` can be reused
    /// across the encoder-reader's hot loop — at 60 fps this avoids
    /// ~60 throwaway `Vec` allocations per second.  Implementations
    /// must not clear `out` (the caller does that between iterations
    /// when appropriate).
    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>);

    /// Number of bytes currently held in the splitter's internal buffer
    /// (i.e. bytes received from the encoder that have not yet been
    /// emitted as a complete access unit).  Used purely for diagnostic
    /// logging — a steadily-growing value while `frames_out` stays
    /// constant indicates that access-unit boundaries are no longer
    /// being recognised in the encoder's output (e.g. AUD NAL units
    /// missing from H.264 / HEVC streams, or temporal-delimiter OBUs
    /// missing from AV1 LOBF streams).
    fn buffered_bytes(&self) -> usize;
}

// ── H.264 splitter ─────────────────────────────────────────────────

/// Splits an H.264 Annex-B byte-stream into access units by detecting
/// AUD NAL units (nal_unit_type == 9).
///
/// Uses an index-based read cursor (`read_pos`) rather than draining the
/// front of `buf` after every emitted access unit — `Vec::drain(..n)`
/// memmoves the residual bytes (typically a partial next frame) to the
/// vector's start, and at 60 fps with 100–500 KB IDRs that backlog of
/// memcpy contends for the same memory bandwidth as the encoder-reader
/// thread that is concurrently draining FFmpeg's stdout.  Compacting is
/// instead performed lazily, only when the consumed prefix exceeds half
/// of the buffer length, so the worst-case compaction work is bounded
/// by the live (still-needed) bytes.
pub struct H264Splitter {
    buf: Vec<u8>,
    /// First index in `buf` that has not yet been emitted as part of an
    /// access unit.  All bytes in `buf[..read_pos]` are dead.
    read_pos: usize,
}

impl H264Splitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    /// Compact `buf` if more than half of its content is dead bytes.
    /// O(live_bytes) per compaction, amortised O(1) per emitted frame
    /// (we only compact once we've consumed at least as many bytes as
    /// remain).
    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    /// Find the byte offset of the next AUD start-code at or after `from`.
    ///
    /// Uses `memchr` to skip ahead to the next `0x00` byte (SIMD-accelerated
    /// on x86_64) and only then validates the surrounding start-code
    /// pattern.  This is dramatically faster on large I-frames than the
    /// previous byte-by-byte scan.
    ///
    /// `from` is interpreted in absolute buffer coordinates (post-`read_pos`).
    fn find_aud(&self, from: usize) -> Option<usize> {
        let d = &self.buf;
        let mut i = from;
        while i + 3 < d.len() {
            // Jump to the next zero byte.
            let rel = memchr::memchr(0, &d[i..d.len().saturating_sub(3)])?;
            i += rel;
            if i + 3 >= d.len() {
                return None;
            }
            if d[i + 1] == 0 {
                // 4-byte start-code: 00 00 00 01 <nal type 9>
                if i + 4 < d.len()
                    && d[i + 2] == 0
                    && d[i + 3] == 1
                    && (d[i + 4] & 0x1F) == 9
                {
                    return Some(i);
                }
                // 3-byte start-code: 00 00 01 <nal type 9>
                if d[i + 2] == 1 && (d[i + 3] & 0x1F) == 9 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    /// Returns true if the access-unit data contains an IDR slice (type 5).
    fn contains_idr(data: &[u8]) -> bool {
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                let nal_idx = if data[i + 2] == 1 {
                    i + 3
                } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if nal_idx < data.len() && (data[nal_idx] & 0x1F) == 5 {
                    return true;
                }
                i = nal_idx;
            } else {
                i += 1;
            }
        }
        false
    }
}

impl FrameSplitter for H264Splitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);

        let mut search = self.read_pos;
        let mut prev_aud: Option<usize> = None;

        while search + 3 < self.buf.len() {
            if let Some(aud_pos) = self.find_aud(search) {
                if let Some(start) = prev_aud {
                    let au_slice = &self.buf[start..aud_pos];
                    if !au_slice.is_empty() {
                        let is_key = Self::contains_idr(au_slice);
                        let mut data = Vec::with_capacity(
                            EncodedFrame::HEADER_LEN + au_slice.len(),
                        );
                        data.resize(EncodedFrame::HEADER_LEN, 0);
                        data.extend_from_slice(au_slice);
                        out.push(EncodedFrame { data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        if let Some(start) = prev_aud {
            self.read_pos = start;
            self.compact();
        }
    }
}

// ── HEVC splitter ──────────────────────────────────────────────────

/// Splits an HEVC Annex-B byte-stream into access units by detecting
/// AUD NAL units (nal_unit_type == 35).  Unlike H.264, HEVC uses a
/// 2-byte NAL header where the type lives in bits 1–6 of the first
/// byte (the leading bit is `forbidden_zero_bit`).
///
/// See [`H264Splitter`] for the rationale behind the index-based read
/// cursor (`read_pos`) instead of `Vec::drain`.
pub struct HevcSplitter {
    buf: Vec<u8>,
    read_pos: usize,
}

impl HevcSplitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    /// See [`H264Splitter::compact`].
    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    /// HEVC NAL header byte 0 → `nal_unit_type` (6 bits, after the
    /// `forbidden_zero_bit`).
    fn hevc_nal_type(b0: u8) -> u8 {
        (b0 >> 1) & 0x3F
    }

    fn find_aud(&self, from: usize) -> Option<usize> {
        let d = &self.buf;
        let mut i = from;
        while i + 3 < d.len() {
            let rel = memchr::memchr(0, &d[i..d.len().saturating_sub(3)])?;
            i += rel;
            if i + 3 >= d.len() {
                return None;
            }
            if d[i + 1] == 0 {
                // 4-byte start-code: 00 00 00 01 <nal[0]>
                if i + 4 < d.len()
                    && d[i + 2] == 0
                    && d[i + 3] == 1
                    && Self::hevc_nal_type(d[i + 4]) == 35
                {
                    return Some(i);
                }
                // 3-byte start-code: 00 00 01 <nal[0]>
                if d[i + 2] == 1 && Self::hevc_nal_type(d[i + 3]) == 35 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    /// True if the access-unit contains an IDR (NAL type 19 or 20).
    /// We also accept the broader IRAP range 16..=23 as a key-frame
    /// hint so CRA / BLA frames in HEVC streams are still flagged for
    /// decoder reset.
    fn contains_keyframe(data: &[u8]) -> bool {
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                let nal_idx = if data[i + 2] == 1 {
                    i + 3
                } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if nal_idx < data.len() {
                    let t = Self::hevc_nal_type(data[nal_idx]);
                    if (16..=23).contains(&t) {
                        return true;
                    }
                }
                i = nal_idx;
            } else {
                i += 1;
            }
        }
        false
    }
}

impl FrameSplitter for HevcSplitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);

        let mut search = self.read_pos;
        let mut prev_aud: Option<usize> = None;

        while search + 3 < self.buf.len() {
            if let Some(aud_pos) = self.find_aud(search) {
                if let Some(start) = prev_aud {
                    let au_slice = &self.buf[start..aud_pos];
                    if !au_slice.is_empty() {
                        let is_key = Self::contains_keyframe(au_slice);
                        let mut data = Vec::with_capacity(
                            EncodedFrame::HEADER_LEN + au_slice.len(),
                        );
                        data.resize(EncodedFrame::HEADER_LEN, 0);
                        data.extend_from_slice(au_slice);
                        out.push(EncodedFrame { data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        if let Some(start) = prev_aud {
            self.read_pos = start;
            self.compact();
        }
    }
}

// ── AV1 splitter (Low-Overhead Bitstream Format) ───────────────────

/// AV1 OBU types we care about.  Defined by the AV1 spec, section 6.2.1.
/// `OBU_FRAME` and `OBU_FRAME_HEADER` are kept as named constants for
/// documentation even though the splitter only uses them implicitly via
/// the `is_keyframe` heuristic on Sequence Header presence.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
#[allow(dead_code)]
const OBU_FRAME_HEADER: u8 = 3;
#[allow(dead_code)]
const OBU_FRAME: u8 = 6;

/// Splits an AV1 Low-Overhead Bitstream Format stream into access
/// units by detecting Temporal Delimiter OBUs (type 2).  Each TD marks
/// the start of a new temporal unit (i.e. one displayed picture in our
/// single-layer output).
///
/// Each OBU is laid out as:
/// ```text
///   obu_header               (1 byte: f(1)=0 | type(4) | ext_flag(1) | size_flag(1) | reserved(1))
///   obu_extension_header     (1 byte, if ext_flag set)
///   obu_size                 (LEB128, if size_flag set; FFmpeg's
///                             `-f obu` muxer always sets size_flag)
///   payload
/// ```
pub struct Av1Splitter {
    buf: Vec<u8>,
    /// See [`H264Splitter`] for the rationale.
    read_pos: usize,
}

impl Av1Splitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    /// See [`H264Splitter::compact`].
    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    /// Parse a LEB128 (variable-length unsigned integer, max 8 bytes
    /// per AV1 spec §4.10.5).  Returns `(value, byte_count)` or `None`
    /// if the buffer is truncated or the encoding is invalid.
    ///
    /// The 8-byte cap also makes the function trivially overflow-safe:
    /// the highest shift used is `7 * 7 = 49` (during the 8th iteration
    /// `shift` is set to 49 *after* the last shift), well below `u64`'s
    /// 64-bit width — so no `shift >= 64` guard is needed.
    fn read_leb128(buf: &[u8]) -> Option<(u64, usize)> {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        for (i, &b) in buf.iter().enumerate().take(8) {
            value |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Some((value, i + 1));
            }
            shift += 7;
        }
        None
    }

    /// Walk the OBU stream and append complete access units to `out`,
    /// plus a flag for whether each contains a key-frame.
    /// Anything left over (incomplete trailing OBU) stays in `self.buf`.
    fn extract_units(&mut self, out: &mut Vec<EncodedFrame>) {
        let mut td_starts: Vec<usize> = Vec::new();
        let mut keyframe_units: Vec<bool> = Vec::new();
        let mut current_is_key = false;

        let mut pos = self.read_pos;
        let mut last_complete_end = self.read_pos;

        while pos < self.buf.len() {
            let header = self.buf[pos];
            let obu_type = (header >> 3) & 0x0F;
            let ext_flag = (header & 0x04) != 0;
            let size_flag = (header & 0x02) != 0;

            let mut p = pos + 1;
            if ext_flag {
                if p >= self.buf.len() {
                    break;
                }
                p += 1;
            }

            // FFmpeg's `-f obu` muxer always emits size-prefixed OBUs.
            // If we ever encounter an OBU without a size we treat the
            // remainder of the buffer as the payload (degenerate case).
            let payload_len = if size_flag {
                if p >= self.buf.len() {
                    break;
                }
                let (len, n) = match Self::read_leb128(&self.buf[p..]) {
                    Some(v) => v,
                    None => break, // need more data
                };
                p += n;
                len as usize
            } else {
                self.buf.len().saturating_sub(p)
            };

            let payload_end = p.checked_add(payload_len);
            let payload_end = match payload_end {
                Some(v) if v <= self.buf.len() => v,
                _ => break, // need more data
            };

            // Track key-frame status for the *current* temporal unit.
            // We treat the appearance of a Sequence Header OBU as a
            // strong key-frame signal: encoders emit it before every
            // IDR.  `frame_header.frame_type == KEY_FRAME` parsing is
            // intentionally avoided here because it would require
            // building a full bit-reader and tracking the show_frame
            // logic — overkill given the SH heuristic is what every
            // production AV1 splitter (including dav1d, gstreamer) uses
            // as a fast keyframe oracle.
            if obu_type == OBU_SEQUENCE_HEADER {
                current_is_key = true;
            }

            if obu_type == OBU_TEMPORAL_DELIMITER {
                td_starts.push(pos);
                keyframe_units.push(false);
                current_is_key = false;
            } else if let Some(last) = keyframe_units.last_mut() {
                if current_is_key {
                    *last = true;
                }
            }

            pos = payload_end;
            last_complete_end = pos;
        }

        // Emit one frame per pair of adjacent TDs.  The bytes between
        // TD[i] and TD[i+1] (exclusive of TD[i+1]) form one access unit.
        if td_starts.len() >= 2 {
            for w in 0..td_starts.len() - 1 {
                let start = td_starts[w];
                let end = td_starts[w + 1];
                let au_slice = &self.buf[start..end];
                if !au_slice.is_empty() {
                    let mut data = Vec::with_capacity(
                        EncodedFrame::HEADER_LEN + au_slice.len(),
                    );
                    data.resize(EncodedFrame::HEADER_LEN, 0);
                    data.extend_from_slice(au_slice);
                    out.push(EncodedFrame {
                        data,
                        is_keyframe: keyframe_units[w],
                    });
                }
            }
            // Advance the read cursor to the last (still-open) TD; lazy
            // compaction keeps the underlying buffer from growing.
            self.read_pos = *td_starts.last().unwrap();
            self.compact();
        } else if last_complete_end > self.read_pos && td_starts.is_empty() {
            // Defensive: if we somehow consumed bytes without seeing a
            // TD, advance the cursor so the buffer doesn't grow unbounded.
            self.read_pos = last_complete_end;
            self.compact();
        }
    }
}

impl FrameSplitter for Av1Splitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);
        self.extract_units(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Annex-B fragment: AUD (4-byte SC, type 9) +
    /// `payload` bytes (the slice should not contain another AUD start).
    fn aud_au(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09]; // AUD
        v.extend_from_slice(payload);
        v
    }

    /// Test helper: invoke `splitter.push` and return the access units
    /// it appended, instead of having every test plumb its own `Vec`.
    fn push_collect(s: &mut dyn FrameSplitter, data: &[u8]) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        s.push(data, &mut out);
        out
    }

    #[test]
    fn h264_detects_two_access_units_split_on_aud() {
        let mut det = H264Splitter::new();
        // Two AUs back-to-back: AUD | non-IDR slice (type 1)
        //                     + AUD | IDR slice     (type 5)
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]); // type 1 (P/B)
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb])); // type 5 (IDR)
        // Trailing AUD so the second AU is closed.
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].is_keyframe);
        assert!(frames[1].is_keyframe);
        // Each emitted frame must reserve the 10-byte wire header.
        for f in &frames {
            assert!(f.data.len() >= EncodedFrame::HEADER_LEN);
            // Header bytes start out zeroed.
            assert!(f.data[..EncodedFrame::HEADER_LEN].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn h264_handles_streaming_chunks_without_losing_data() {
        // Same bytes as above, fed one byte at a time.  Stresses the
        // `drain`-based leftover handling.
        let mut det = H264Splitter::new();
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]);
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(!got[0].is_keyframe);
        assert!(got[1].is_keyframe);
    }

    #[test]
    fn h264_no_aud_means_no_frames_emitted() {
        let mut det = H264Splitter::new();
        let frames = push_collect(&mut det, &[0x00, 0x00, 0x00, 0x01, 0x65, 0xff, 0xff]);
        assert!(frames.is_empty());
    }

    /// Drives the lazy-compaction path: feed many access units in
    /// sequence and verify that the splitter's internal buffer doesn't
    /// grow without bound.  Each push leaves only the trailing AUD
    /// (5 bytes) plus partial next-frame payload as live data, so after
    /// many iterations the live region must remain small.
    #[test]
    fn h264_lazy_compaction_keeps_buffer_bounded() {
        let mut det = H264Splitter::new();
        // Build a single AU large enough that the dead-prefix exceeds
        // the live tail and triggers compaction every iteration.
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x09]; // AUD
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]); // NAL type 1
        au.extend(std::iter::repeat_n(0xaau8, 4096));    // big payload
        let trailing_aud = [0x00, 0x00, 0x00, 0x01, 0x09];

        // Prime with one open AU.
        push_collect(&mut det, &au);

        // Feed 100 more (closing AUDs) — each push completes the prior
        // AU and starts a new one.  Without compaction the buffer would
        // grow to ~400 KB.
        for _ in 0..100 {
            // Closing AUD for the previous AU + a fresh AU body.
            let mut chunk = trailing_aud.to_vec();
            chunk.extend_from_slice(&au[5..]); // body without leading AUD
            let frames = push_collect(&mut det, &chunk);
            assert_eq!(frames.len(), 1);
        }

        // After 100 iterations the live buffer must be O(one frame),
        // not O(100 frames).  Allow generous headroom.
        assert!(
            det.buffered_bytes() < 16 * 1024,
            "buffer grew unboundedly: {} bytes",
            det.buffered_bytes()
        );
    }

    /// Build a minimal HEVC AU: AUD (4-byte SC, type 35) + `payload`.
    /// The HEVC AUD NAL header byte 0 is `0x46` because `type 35 << 1 = 70 = 0x46`.
    fn hevc_aud_au(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x46, 0x01]; // AUD + 2-byte NAL header
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn hevc_detects_two_access_units() {
        let mut det = HevcSplitter::new();
        // Trailing-picture (TRAIL_R, type 1 → byte 0 = 0x02): non-key
        let mut bytes = hevc_aud_au(&[0x00, 0x00, 0x01, 0x02, 0x01, 0xaa]);
        // IDR_W_RADL (type 19 → byte 0 = 0x26): key
        bytes.extend_from_slice(&hevc_aud_au(&[0x00, 0x00, 0x01, 0x26, 0x01, 0xbb]));
        // Trailing AUD so the second AU is closed.
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x46, 0x01]);

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].is_keyframe);
        assert!(frames[1].is_keyframe);
    }

    #[test]
    fn hevc_streamed_one_byte_at_a_time() {
        let mut det = HevcSplitter::new();
        let mut bytes = hevc_aud_au(&[0x00, 0x00, 0x01, 0x02, 0x01, 0xaa]);
        bytes.extend_from_slice(&hevc_aud_au(&[0x00, 0x00, 0x01, 0x26, 0x01, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x46, 0x01]);

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(!got[0].is_keyframe);
        assert!(got[1].is_keyframe);
    }

    /// Build one OBU with size flag set: header byte | LEB128(size) | payload.
    fn av1_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        // header = 0_TTTT_0_1_0 (type, no extension, size flag set)
        let header = (obu_type & 0x0F) << 3 | 0x02;
        let mut v = vec![header];
        // LEB128 of payload length (single byte for length < 128).
        assert!(payload.len() < 128, "test helper only handles small OBUs");
        v.push(payload.len() as u8);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn av1_detects_two_access_units() {
        let mut det = Av1Splitter::new();
        let mut bytes = Vec::new();
        // First TU: TD + sequence header + frame
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x01]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xAA, 0xBB, 0xCC]));
        // Second TU: TD + frame (no SH → not a keyframe)
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xDD, 0xEE]));
        // Third TD so the second TU is closed.
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_keyframe, "TU containing a Sequence Header must be flagged as keyframe");
        assert!(!frames[1].is_keyframe);
        for f in &frames {
            assert!(f.data.len() >= EncodedFrame::HEADER_LEN);
        }
    }

    #[test]
    fn av1_streamed_one_byte_at_a_time() {
        let mut det = Av1Splitter::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x01]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xAA, 0xBB]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xCC]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(got[0].is_keyframe);
        assert!(!got[1].is_keyframe);
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
        assert_eq!(CodecKind::from_encoder_name("unknown"), CodecKind::H264); // safe default
    }

    #[test]
    fn codec_kind_protocol_id_stable() {
        assert_eq!(CodecKind::H264.protocol_id(), 0);
        assert_eq!(CodecKind::Hevc.protocol_id(), 1);
        assert_eq!(CodecKind::Av1.protocol_id(), 2);
    }

    #[test]
    fn ffmpeg_stderr_hint_detects_av1_amf_unsupported() {
        // The exact line emitted by FFmpeg when the GPU/driver lacks
        // an AV1 hardware encoder, captured from a real run on a
        // pre-RDNA3 AMD card.
        let line = "[av1_amf @ 000002b2687ee4c0] CreateComponent(AMFVideoEncoderHW_AV1) \
                    failed with error 30";
        let hint = ffmpeg_stderr_hint(line).expect("must produce a hint");
        assert!(hint.contains("RDNA3"), "hint should mention HW requirement");
        assert!(hint.contains("--encoder"), "hint should suggest a switch");
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
    fn av1_leb128_round_trip_small_and_multi_byte() {
        // single-byte values (0..=127)
        assert_eq!(Av1Splitter::read_leb128(&[0x00]), Some((0, 1)));
        assert_eq!(Av1Splitter::read_leb128(&[0x7F]), Some((127, 1)));
        // two-byte: 200 = 11001000 → encoded LSB-first 7-bit groups
        // 200 = 0b1100_1000 → group0 = 0b1001000 (0x48) | 0x80, group1 = 0b0000001 (0x01)
        assert_eq!(Av1Splitter::read_leb128(&[0xC8, 0x01]), Some((200, 2)));
    }
}
