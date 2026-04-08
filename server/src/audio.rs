use std::io::Read;
use std::process::{Command, Stdio};
use tokio::sync::mpsc;

/// Size of one audio chunk in bytes: 20 ms of 48 kHz stereo f32le.
/// 960 samples × 2 channels × 4 bytes = 7680 bytes.
const CHUNK_BYTES: usize = 960 * 2 * 4;

/// Enumerate available audio capture devices using FFmpeg.
///
/// On Windows this runs `ffmpeg -list_devices true -f dshow -i dummy` and
/// parses the DirectShow audio device names from stderr.
///
/// On Linux this runs `ffmpeg -sources pulse` and parses PulseAudio source
/// names from stderr.
///
/// Returns a list of human-readable device names.  The order is stable
/// (FFmpeg lists them deterministically) so the caller can use the index
/// to refer back to a specific device.
pub fn enumerate_audio_devices() -> Vec<String> {
    #[cfg(windows)]
    {
        enumerate_audio_devices_dshow()
    }
    #[cfg(not(windows))]
    {
        enumerate_audio_devices_pulse()
    }
}

/// Windows: parse DirectShow audio device names.
#[cfg(windows)]
fn enumerate_audio_devices_dshow() -> Vec<String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            log::warn!("Failed to run FFmpeg for audio device enumeration: {e}");
            return Vec::new();
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_dshow_audio_devices(&stderr)
}

/// Parse audio device names from FFmpeg dshow output.
///
/// The output looks like:
/// ```text
/// [dshow @ ...] DirectShow video devices (...)
/// [dshow @ ...]  "Integrated Camera"
/// [dshow @ ...] Alternative name "..."
/// [dshow @ ...] DirectShow audio devices
/// [dshow @ ...]  "Stereo Mix (Realtek ...)"
/// [dshow @ ...] Alternative name "..."
/// [dshow @ ...]  "Microphone (Realtek ...)"
/// [dshow @ ...] Alternative name "..."
/// ```
///
/// We look for lines after "DirectShow audio devices" that contain a
/// quoted device name (but skip "Alternative name" lines).
#[cfg(any(windows, test))]
fn parse_dshow_audio_devices(stderr: &str) -> Vec<String> {
    let mut devices = Vec::new();
    let mut in_audio_section = false;

    for line in stderr.lines() {
        // Strip the `[dshow @ 0x...]` prefix to get the payload.
        let payload = match line.find(']') {
            Some(pos) => line[pos + 1..].trim(),
            None => line.trim(),
        };

        if payload.contains("DirectShow audio devices") {
            in_audio_section = true;
            continue;
        }
        // A new section header ends the audio section.
        if payload.contains("DirectShow video devices") {
            in_audio_section = false;
            continue;
        }

        if !in_audio_section {
            continue;
        }

        // Skip "Alternative name" lines.
        if payload.starts_with("Alternative name") {
            continue;
        }

        // Extract the quoted device name.
        if let (Some(start), Some(end)) = (payload.find('"'), payload.rfind('"')) {
            if start < end {
                let name = &payload[start + 1..end];
                if !name.is_empty() {
                    devices.push(name.to_string());
                }
            }
        }
    }

    devices
}

/// Linux: parse PulseAudio source names.
#[cfg(not(windows))]
fn enumerate_audio_devices_pulse() -> Vec<String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-sources", "pulse"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            log::warn!("Failed to run FFmpeg for audio device enumeration: {e}");
            return Vec::new();
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_pulse_sources(&stderr)
}

/// Parse PulseAudio source names from `ffmpeg -sources pulse` output.
///
/// Output looks like:
/// ```text
/// Auto-detected sources for pulse:
///   * default [Default]
///     alsa_output.pci-0000_00_1f.3.analog-stereo.monitor [Monitor of ...]
/// ```
#[cfg(not(windows))]
fn parse_pulse_sources(stderr: &str) -> Vec<String> {
    let mut devices = Vec::new();
    let mut in_sources = false;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Auto-detected sources") {
            in_sources = true;
            continue;
        }
        if !in_sources {
            continue;
        }
        // Each source line starts with optional `*` then the source name.
        let source_part = trimmed.trim_start_matches('*').trim();
        if source_part.is_empty() {
            continue;
        }
        // The name ends at `[` or end-of-line.
        let name = match source_part.find('[') {
            Some(pos) => source_part[..pos].trim(),
            None => source_part,
        };
        if !name.is_empty() {
            devices.push(name.to_string());
        }
    }

    devices
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dshow_audio_devices_typical() {
        let stderr = r#"[dshow @ 000001] DirectShow video devices (some may be both video and audio devices)
[dshow @ 000001]  "Integrated Camera"
[dshow @ 000001] Alternative name "@device_pnp_\\?\usb"
[dshow @ 000001] DirectShow audio devices
[dshow @ 000001]  "Stereo Mix (Realtek High Definition Audio)"
[dshow @ 000001] Alternative name "@device_cm_{id1}"
[dshow @ 000001]  "Microphone (Realtek High Definition Audio)"
[dshow @ 000001] Alternative name "@device_cm_{id2}"
"#;
        let devices = parse_dshow_audio_devices(stderr);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], "Stereo Mix (Realtek High Definition Audio)");
        assert_eq!(devices[1], "Microphone (Realtek High Definition Audio)");
    }

    #[test]
    fn parse_dshow_audio_devices_no_audio() {
        let stderr = r#"[dshow @ 000001] DirectShow video devices (some may be both video and audio devices)
[dshow @ 000001]  "Integrated Camera"
"#;
        let devices = parse_dshow_audio_devices(stderr);
        assert!(devices.is_empty());
    }

    #[test]
    fn parse_dshow_audio_devices_empty() {
        let devices = parse_dshow_audio_devices("");
        assert!(devices.is_empty());
    }
}
