use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use tokio::sync::mpsc;

/// One encoded video frame ready to send to the client.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// Manages an FFmpeg subprocess that accepts raw BGRA frames on stdin
/// and produces an H.264 Annex-B byte-stream on stdout.
pub struct FfmpegEncoder {
    #[allow(dead_code)]
    process: Child,
    stdin: BufWriter<std::process::ChildStdin>,
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

        Ok(Self { process, stdin })
    }

    /// Write one raw BGRA frame into FFmpeg's stdin.
    pub fn send_frame(&mut self, bgra: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.stdin.write_all(bgra)?;
        self.stdin.flush()?;
        Ok(())
    }
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
                    let au_data = self.buf[start..aud_pos].to_vec();
                    if !au_data.is_empty() {
                        let is_key = Self::contains_idr(&au_data);
                        frames.push(EncodedFrame { data: au_data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        // Keep only unprocessed data in the buffer.
        if let Some(start) = prev_aud {
            self.buf = self.buf[start..].to_vec();
        }

        frames
    }

    /// Find the byte offset of the next AUD start-code at or after `from`.
    fn find_aud(&self, from: usize) -> Option<usize> {
        let d = &self.buf;
        let mut i = from;
        while i + 3 < d.len() {
            if d[i] == 0 && d[i + 1] == 0 {
                // 4-byte start-code
                if i + 4 < d.len() && d[i + 2] == 0 && d[i + 3] == 1 && (d[i + 4] & 0x1F) == 9
                {
                    return Some(i);
                }
                // 3-byte start-code
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
