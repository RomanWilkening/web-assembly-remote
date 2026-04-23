use std::io::Read;
use std::process::{Command, Stdio};
use tokio::sync::mpsc;

/// Size of one audio chunk in bytes: 20 ms of 48 kHz stereo f32le.
/// 960 samples × 2 channels × 4 bytes = 7680 bytes.
const CHUNK_BYTES: usize = 960 * 2 * 4;

// ═══════════════════════════════════════════════════════════════════
// Windows: WASAPI loopback capture
//
// Instead of listing audio *input* devices (microphones, Stereo Mix)
// via FFmpeg/DirectShow, we enumerate audio *render* endpoints
// (speakers, headphones) using WASAPI.  Capture uses WASAPI loopback
// mode which records whatever the system is playing through the
// selected output device – no "Stereo Mix" or virtual cable required.
// ═══════════════════════════════════════════════════════════════════

/// Enumerate available audio output (render) devices.
///
/// On Windows this uses WASAPI to list active render endpoints
/// (speakers, headphones, etc.).  The returned names can be passed
/// to [`audio_capture_loop`] to start loopback capture on that device.
///
/// On Linux this parses PulseAudio monitor sources from
/// `pactl list sources short`.
#[cfg(windows)]
pub fn enumerate_audio_devices() -> Vec<String> {
    enumerate_audio_devices_wasapi()
}

#[cfg(not(windows))]
pub fn enumerate_audio_devices() -> Vec<String> {
    enumerate_audio_devices_pulse()
}

/// Capture system audio via WASAPI loopback (Windows) or FFmpeg/PulseAudio
/// (Linux) and send raw f32le PCM chunks through the provided channel.
///
/// On Windows the `audio_device` parameter is the friendly name of a
/// render endpoint (e.g. "Speakers (Realtek …)") or `"default"` for
/// the system default output.  The function captures whatever audio
/// is being played through that device.
///
/// This function blocks and should be called from a dedicated thread
/// (e.g. `tokio::task::spawn_blocking`).
#[cfg(windows)]
pub fn audio_capture_loop(audio_device: &str, audio_tx: mpsc::Sender<Vec<u8>>) {
    if let Err(e) = run_wasapi_loopback(audio_device, &audio_tx) {
        log::error!("WASAPI loopback capture error: {e}");
    }
}

#[cfg(not(windows))]
pub fn audio_capture_loop(audio_device: &str, audio_tx: mpsc::Sender<Vec<u8>>) {
    if let Err(e) = run_ffmpeg_capture(audio_device, &audio_tx) {
        log::error!("Audio capture error: {e}");
    }
}

// ── Windows WASAPI implementation ──────────────────────────────────

#[cfg(windows)]
fn enumerate_audio_devices_wasapi() -> Vec<String> {
    // Initialize COM for this thread (MTA).
    let _ = wasapi::initialize_mta();

    let enumerator = match wasapi::DeviceEnumerator::new() {
        Ok(e) => e,
        Err(e) => {
            log::warn!("Failed to create WASAPI device enumerator: {e}");
            return Vec::new();
        }
    };

    let collection = match enumerator.get_device_collection(&wasapi::Direction::Render) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to enumerate WASAPI render devices: {e}");
            return Vec::new();
        }
    };

    let count = match collection.get_nbr_devices() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to get render device count: {e}");
            return Vec::new();
        }
    };

    let mut devices = Vec::new();
    for i in 0..count {
        match collection.get_device_at_index(i) {
            Ok(device) => match device.get_friendlyname() {
                Ok(name) => devices.push(name),
                Err(e) => log::debug!("Skipping render device {i}: cannot read name: {e}"),
            },
            Err(e) => log::debug!("Skipping render device {i}: {e}"),
        }
    }

    devices
}

#[cfg(windows)]
fn run_wasapi_loopback(
    device_name: &str,
    audio_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::VecDeque;

    // Initialize COM on this thread (HRESULT::ok() → Result).
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM initialization failed: {e}"))?;

    let enumerator = wasapi::DeviceEnumerator::new()?;

    // Resolve the requested device.
    let device = if device_name.eq_ignore_ascii_case("default") {
        enumerator.get_default_device(&wasapi::Direction::Render)?
    } else {
        let collection = enumerator.get_device_collection(&wasapi::Direction::Render)?;
        collection.get_device_with_name(device_name)?
    };

    let friendly = device.get_friendlyname().unwrap_or_default();
    log::info!("WASAPI loopback: opening device \"{friendly}\"");

    let mut audio_client = device.get_iaudioclient()?;

    // We want 48 kHz stereo f32le – with autoconvert WASAPI will
    // resample from the device's native format automatically.
    let desired_format = wasapi::WaveFormat::new(
        32,                         // bits per sample
        32,                         // valid bits per sample
        &wasapi::SampleType::Float, // f32
        48000,                      // sample rate
        2,                          // channels (stereo)
        None,                       // channel mask (auto)
    );

    let blockalign = desired_format.get_blockalign() as usize;

    let (def_time, _min_time) = audio_client.get_device_period()?;

    // Event-driven shared mode with automatic format conversion.
    // Passing Direction::Capture on a Render device makes the wasapi
    // crate set AUDCLNT_STREAMFLAGS_LOOPBACK automatically.
    let mode = wasapi::StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_time,
    };

    audio_client.initialize_client(
        &desired_format,
        &wasapi::Direction::Capture,
        &mode,
    )?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;

    audio_client.start_stream()?;
    log::info!("WASAPI loopback capture started (48 kHz stereo f32le, blockalign={blockalign})");

    let mut sample_queue: VecDeque<u8> = VecDeque::with_capacity(CHUNK_BYTES * 8);

    loop {
        // Read whatever the device has buffered.
        match capture_client.read_from_device_to_deque(&mut sample_queue) {
            Ok(_) => {}
            Err(e) => {
                log::error!("WASAPI read error: {e}");
                break;
            }
        }

        // Drain complete chunks from the queue and send them.
        while sample_queue.len() >= CHUNK_BYTES {
            // `make_contiguous` returns a single &mut [u8] view of the
            // queue's internal storage so we can copy CHUNK_BYTES out
            // with a single memcpy instead of `pop_front()`-ing each
            // byte.  At 96 kB/s this saves ~96k VecDeque pops per
            // second; small but unambiguously a win on the audio
            // hot-path.
            //
            // We allocate the destination buffer with `to_vec()` rather
            // than `vec![0u8; CHUNK_BYTES]` + copy: the latter does a
            // 7.7 kB memset per 20 ms tick (~385 kB/s of zeroing) that
            // is immediately overwritten by `copy_from_slice`.  Going
            // through `to_vec()` collapses the allocation + memcpy into
            // a single memory-bandwidth pass and keeps the audio thread
            // out of the allocator's slow path that would otherwise
            // contend with the video hot-path's allocations.
            let chunk = {
                let head = sample_queue.make_contiguous();
                head[..CHUNK_BYTES].to_vec()
            };
            sample_queue.drain(..CHUNK_BYTES);
            if audio_tx.blocking_send(chunk).is_err() {
                log::info!("Audio channel closed – stopping WASAPI loopback");
                let _ = audio_client.stop_stream();
                return Ok(());
            }
        }

        // Wait for next buffer event (timeout 2 s).
        if h_event.wait_for_event(2_000_000).is_err() {
            // Check whether the channel is still alive.
            if audio_tx.is_closed() {
                log::info!("Audio channel closed – stopping WASAPI loopback");
                break;
            }
            // Otherwise it's just a timeout; retry.
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

// ── Linux PulseAudio implementation ───────────────────────────────

/// Linux: list PulseAudio monitor sources (these capture audio output).
///
/// Runs `pactl list sources short` and picks sources whose name
/// contains `.monitor` – these correspond to the audio being played
/// through each PulseAudio sink (output device).
///
/// Falls back to parsing `ffmpeg -sources pulse` if `pactl` is not
/// available.
#[cfg(not(windows))]
fn enumerate_audio_devices_pulse() -> Vec<String> {
    // Try pactl first (more reliable).
    if let Some(devices) = enumerate_via_pactl() {
        if !devices.is_empty() {
            return devices;
        }
    }

    // Fallback: use FFmpeg.
    enumerate_via_ffmpeg_pulse()
}

/// Enumerate PulseAudio monitor sources via `pactl`.
#[cfg(not(windows))]
fn enumerate_via_pactl() -> Option<Vec<String>> {
    let output = Command::new("pactl")
        .args(["list", "sources", "short"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let devices = parse_pactl_sources(&stdout);
    Some(devices)
}

/// Parse `pactl list sources short` output.
///
/// Each line looks like:
/// ```text
/// 0   alsa_output.pci-0000_00_1f.3.analog-stereo.monitor  ...
/// 1   alsa_input.pci-0000_00_1f.3.analog-stereo            ...
/// ```
///
/// We return all source names (both monitors and inputs) so the user
/// can choose, but monitors (which capture output audio) are listed
/// first.
#[cfg(not(windows))]
fn parse_pactl_sources(stdout: &str) -> Vec<String> {
    let mut monitors = Vec::new();
    let mut others = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let name = parts[1].to_string();
            if name.contains(".monitor") {
                monitors.push(name);
            } else {
                others.push(name);
            }
        }
    }

    // Monitors first (these capture system audio output).
    monitors.extend(others);
    monitors
}

/// Fallback: enumerate PulseAudio sources via FFmpeg.
#[cfg(not(windows))]
fn enumerate_via_ffmpeg_pulse() -> Vec<String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-sources", "pulse"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            log::warn!(
                "Failed to run FFmpeg for audio device enumeration \
                 (ensure FFmpeg or pactl is installed): {e}"
            );
            return Vec::new();
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_ffmpeg_pulse_sources(&stderr)
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
fn parse_ffmpeg_pulse_sources(stderr: &str) -> Vec<String> {
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
        let source_part = trimmed.trim_start_matches('*').trim();
        if source_part.is_empty() {
            continue;
        }
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

/// Linux: capture audio via FFmpeg + PulseAudio.
#[cfg(not(windows))]
fn run_ffmpeg_capture(
    audio_device: &str,
    audio_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new("ffmpeg");

    cmd.args(["-hide_banner", "-loglevel", "error"]);
    cmd.args(["-f", "pulse", "-i", audio_device]);

    // Output: raw f32le 48 kHz stereo on stdout.
    cmd.args(["-f", "f32le", "-ar", "48000", "-ac", "2", "pipe:1"]);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    log::info!("Starting audio capture via FFmpeg: device=\"{audio_device}\"");

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

    #[cfg(not(windows))]
    #[test]
    fn parse_pactl_sources_typical() {
        let output = "\
0\talsa_output.pci-0000_00_1f.3.analog-stereo.monitor\tPipeWire\ts16le 2ch 48000Hz\tIDLE
1\talsa_input.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts16le 2ch 48000Hz\tIDLE
";
        let devices = parse_pactl_sources(output);
        assert_eq!(devices.len(), 2);
        // Monitors come first.
        assert!(devices[0].contains(".monitor"));
        assert_eq!(
            devices[0],
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        );
        assert_eq!(devices[1], "alsa_input.pci-0000_00_1f.3.analog-stereo");
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_pactl_sources_empty() {
        let devices = parse_pactl_sources("");
        assert!(devices.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_ffmpeg_pulse_sources_typical() {
        let stderr = r#"Auto-detected sources for pulse:
  * default [Default]
    alsa_output.pci-0000_00_1f.3.analog-stereo.monitor [Monitor of Built-in Audio Analog Stereo]
"#;
        let devices = parse_ffmpeg_pulse_sources(stderr);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], "default");
        assert_eq!(
            devices[1],
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_ffmpeg_pulse_sources_empty() {
        let devices = parse_ffmpeg_pulse_sources("");
        assert!(devices.is_empty());
    }
}
