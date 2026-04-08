use std::io::Read;
use std::process::{Command, Stdio};
use tokio::sync::mpsc;

/// Size of one audio chunk in bytes: 20 ms of 48 kHz stereo f32le.
/// 960 samples × 2 channels × 4 bytes = 7680 bytes.
const CHUNK_BYTES: usize = 960 * 2 * 4;

/// Capture system audio via FFmpeg and send raw f32le PCM chunks
/// through the provided channel.
///
/// On Windows the device is opened via DirectShow (`-f dshow`).
/// On other platforms it is opened via PulseAudio (`-f pulse`).
///
/// This function blocks and should be called from a dedicated thread
/// (e.g. `tokio::task::spawn_blocking`).
pub fn audio_capture_loop(
    audio_device: &str,
    audio_tx: mpsc::Sender<Vec<u8>>,
) {
    if let Err(e) = run_capture(audio_device, &audio_tx) {
        log::error!("Audio capture error: {e}");
    }
}

fn run_capture(
    audio_device: &str,
    audio_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new("ffmpeg");

    cmd.args(["-hide_banner", "-loglevel", "error"]);

    // Platform-specific input.
    #[cfg(windows)]
    cmd.args(["-f", "dshow", "-i", &format!("audio={audio_device}")]);

    #[cfg(not(windows))]
    cmd.args(["-f", "pulse", "-i", audio_device]);

    // Output: raw f32le 48 kHz stereo on stdout.
    cmd.args([
        "-f", "f32le",
        "-ar", "48000",
        "-ac", "2",
        "pipe:1",
    ]);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    log::info!("Starting audio capture: device=\"{audio_device}\"");

    let mut process = cmd.spawn().map_err(|e| {
        format!("Failed to start FFmpeg for audio capture – is it installed and in PATH? ({e})")
    })?;

    let mut stdout = process.stdout.take().expect("stdout must be piped");
    let stderr = process.stderr.take().expect("stderr must be piped");

    // Log FFmpeg stderr on a background thread.
    std::thread::Builder::new()
        .name("ffmpeg-audio-stderr".into())
        .spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => log::warn!("FFmpeg audio: {l}"),
                    Err(e) => {
                        log::debug!("FFmpeg audio stderr read error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        })?;

    // Read fixed-size chunks (20 ms each) from FFmpeg stdout.
    loop {
        let mut chunk = vec![0u8; CHUNK_BYTES];
        if read_exact_or_eof(&mut stdout, &mut chunk)? == 0 {
            log::info!("FFmpeg audio stdout closed");
            break;
        }
        if audio_tx.blocking_send(chunk).is_err() {
            log::info!("Audio channel closed – stopping capture");
            break;
        }
    }

    let _ = process.kill();
    let _ = process.wait();
    Ok(())
}

/// Read exactly `buf.len()` bytes, returning 0 on EOF.
fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(0),
            Ok(n) => filled += n,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
