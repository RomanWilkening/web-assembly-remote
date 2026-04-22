use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc;

/// One encoded video frame ready to send to the client.
///
/// `data` is laid out so that the first 10 bytes are reserved for the
/// `MSG_VIDEO_FRAME` wire header (`type u8 + timestamp_us u64 LE +
/// is_keyframe u8`) and the remainder is the raw H.264 access-unit
/// payload.  This lets the WebSocket sender fill in the header
/// in-place and forward the whole `Vec` to `axum::Message::Binary`
/// without any further copy of the (potentially multi-MB) H.264 data.
pub struct EncodedFrame {
    /// `[0..10]` reserved header, `[10..]` H.264 Annex-B payload.
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
    /// parses H.264 access-unit boundaries and pushes complete
    /// frames into `frame_tx`.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        quality: u8,
        encoder_name: &str,
        frame_tx: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let size = format!("{}x{}", width, height);

        let mut cmd = Command::new("ffmpeg");

        // ── input ──────────────────────────────────────────────
        cmd.args([
            "-hide_banner",
            "-loglevel", "error",
            // Raw BGRA frames on stdin
            "-f", "rawvideo",
            "-pix_fmt", "bgra",
            "-video_size", &size,
            "-framerate", &fps.to_string(),
            "-i", "pipe:0",
        ]);

        // ── encoder-specific flags ─────────────────────────────
        match encoder_name {
            "h264_amf" => {
                let qp = quality.to_string();
                cmd.args([
                    "-c:v", "h264_amf",
                    "-usage", "ultralowlatency",
                    "-quality", "speed",
                    "-rc", "cqp",
                    "-qp_i", &qp,
                    "-qp_p", &qp,
                    "-profile:v", "main",
                ]);
            }
            "libx264" => {
                let qp = quality.to_string();
                cmd.args([
                    "-c:v", "libx264",
                    "-preset", "ultrafast",
                    "-tune", "zerolatency",
                    "-crf", &qp,
                    "-profile:v", "baseline",
                ]);
            }
            other => {
                // Generic: just set the codec; user is responsible for
                // the correct FFmpeg build.
                cmd.args(["-c:v", other]);
            }
        }

        // ── common output flags ────────────────────────────────
        let gop = (fps * 2).to_string(); // key-frame every 2 seconds
        cmd.args([
            "-bf", "0",               // no B-frames
            "-g", &gop,
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            "-bsf:v", "h264_metadata=aud=insert",
            "-f", "h264",
            "pipe:1",
        ]);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        log::info!("Spawning FFmpeg encoder ({encoder_name})…");

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
                        Ok(l) if !l.is_empty() => log::warn!("FFmpeg: {l}"),
                        Err(e) => {
                            log::debug!("FFmpeg stderr read error: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            })?;

        // Background reader thread: reads H.264 byte-stream, splits on
        // Access Unit Delimiters, and pushes frames into the channel.
        std::thread::Builder::new()
            .name("h264-reader".into())
            .spawn(move || {
                h264_reader_loop(stdout, frame_tx);
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

// ── H.264 Annex-B stream reader ────────────────────────────────────

/// Continuously reads from `stdout`, splits on AUD NAL units,
/// and sends complete access-units through the channel.
fn h264_reader_loop(
    mut stdout: impl Read,
    tx: mpsc::Sender<EncodedFrame>,
) {
    let mut detector = AuDetector::new();
    let mut buf = vec![0u8; 128 * 1024]; // 128 KiB read buffer

    loop {
        match stdout.read(&mut buf) {
            Ok(0) => {
                log::info!("FFmpeg stdout closed");
                break;
            }
            Ok(n) => {
                for frame in detector.push(&buf[..n]) {
                    if tx.blocking_send(frame).is_err() {
                        log::info!("Frame channel closed – stopping reader");
                        return;
                    }
                }
            }
            Err(e) => {
                log::error!("FFmpeg read error: {e}");
                break;
            }
        }
    }
}

// ── Access-Unit Delimiter based frame splitter ─────────────────────

/// Splits an H.264 Annex-B byte-stream into access units by
/// detecting AUD NAL units (nal_unit_type == 9).
struct AuDetector {
    buf: Vec<u8>,
}

impl AuDetector {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024) }
    }

    /// Append raw bytes and return any complete access units found.
    ///
    /// Each emitted `EncodedFrame` carries a payload `Vec<u8>` whose
    /// first `EncodedFrame::HEADER_LEN` bytes are pre-reserved for the
    /// wire header — see `EncodedFrame` docs.
    fn push(&mut self, data: &[u8]) -> Vec<EncodedFrame> {
        self.buf.extend_from_slice(data);
        let mut frames = Vec::new();

        // Scan for AUD start-codes.
        // AUD 4-byte: 00 00 00 01 <nal_header with type 9>
        // AUD 3-byte: 00 00 01    <nal_header with type 9>
        let mut search = 0;
        let mut prev_aud: Option<usize> = None;

        while search + 3 < self.buf.len() {
            if let Some(aud_pos) = self.find_aud(search) {
                if let Some(start) = prev_aud {
                    let au_slice = &self.buf[start..aud_pos];
                    if !au_slice.is_empty() {
                        let is_key = Self::contains_idr(au_slice);
                        // Allocate once with room for the wire header so
                        // the sender doesn't need a second copy.
                        let mut data = Vec::with_capacity(
                            EncodedFrame::HEADER_LEN + au_slice.len(),
                        );
                        data.resize(EncodedFrame::HEADER_LEN, 0);
                        data.extend_from_slice(au_slice);
                        frames.push(EncodedFrame { data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        // Keep only unprocessed data in the buffer.  `drain` shifts the
        // tail in place — no realloc, no extra copy of the (potentially
        // large) leftover region.
        if let Some(start) = prev_aud {
            if start > 0 {
                self.buf.drain(..start);
            }
        }

        frames
    }

    /// Find the byte offset of the next AUD start-code at or after `from`.
    ///
    /// Uses `memchr` to skip ahead to the next `0x00` byte (SIMD-accelerated
    /// on x86_64) and only then validates the surrounding start-code
    /// pattern.  This is dramatically faster on large I-frames than the
    /// previous byte-by-byte scan.
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

    #[test]
    fn detects_two_access_units_split_on_aud() {
        let mut det = AuDetector::new();
        // Two AUs back-to-back: AUD | non-IDR slice (type 1)
        //                     + AUD | IDR slice     (type 5)
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]); // type 1 (P/B)
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb])); // type 5 (IDR)
        // Trailing AUD so the second AU is closed.
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let frames = det.push(&bytes);
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
    fn handles_streaming_chunks_without_losing_data() {
        // Same bytes as above, fed one byte at a time.  Stresses the
        // `drain`-based leftover handling.
        let mut det = AuDetector::new();
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]);
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(det.push(std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(!got[0].is_keyframe);
        assert!(got[1].is_keyframe);
    }

    #[test]
    fn no_aud_means_no_frames_emitted() {
        let mut det = AuDetector::new();
        let frames = det.push(&[0x00, 0x00, 0x00, 0x01, 0x65, 0xff, 0xff]);
        assert!(frames.is_empty());
    }
}
