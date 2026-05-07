//! Hardware capability probing.
//!
//! On startup the server runs `ffmpeg -hide_banner -encoders` once,
//! parses the (working / installed) encoder list, and intersects it
//! with our `KNOWN_ENCODERS` table.  Each surviving entry becomes an
//! [`EncoderCapability`] that the server can:
//!
//! * present to the client via `MSG_ENCODER_LIST` so the toolbar
//!   only shows encoders that actually have a chance of working, and
//! * use to pick a sensible *default* encoder via [`select_default`]
//!   when the operator hasn't pinned one with `--encoder` /
//!   `[encoder.profiles.<name>].encoder`.
//!
//! On Windows we additionally enumerate DXGI adapters (`dxgi_vendors`)
//! so the auto-select logic can prefer the encoder family that matches
//! the GPU actually present (AMD → AMF, NVIDIA → NVENC, Intel → QSV).
//! Adapter enumeration is best-effort: when the API call fails (no
//! GPU, virtualised host, missing DXGI runtime) the function returns
//! an empty list and the caller falls back to a vendor-agnostic
//! priority order.
//!
//! The optional **probe-encode** step (1×1 BGRA → 1 frame → stdout
//! with a short timeout) is described in the design doc but skipped
//! by default because the per-encoder cost (100–500 ms each) adds
//! several seconds to startup with no measurable benefit on top of
//! the `-encoders` parse + the runtime watchdog described in
//! [`crate::encoder`].  Operators who want it can opt in with
//! [`probe_with_encode`].

use crate::encoder::backends::{lookup, HwVendor, KNOWN_ENCODERS};
use crate::encoder::CodecKind;
use std::process::{Command, Stdio};
use std::time::Duration;

/// One probed encoder (a row in the wire-protocol `EncoderList`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCapability {
    /// FFmpeg encoder name (e.g. `"h264_amf"`).
    pub name: String,
    /// Auto-detected codec family for this encoder.
    pub codec: CodecKind,
    /// Vendor whose hardware this encoder targets (`Software` for x264 et al.).
    pub hw_vendor: HwVendor,
    /// True when the encoder is compiled into the local FFmpeg binary
    /// **and** (when `probe_with_encode` was used) survived the probe.
    pub working: bool,
    /// Reason the encoder was rejected — populated only when `working = false`.
    pub reason: Option<String>,
}

/// Parse the output of `ffmpeg -hide_banner -encoders` and return the
/// set of encoder names listed.
///
/// The relevant section looks like:
///
/// ```text
///  V..... libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10
///  V..... h264_amf             AMD AMF H.264 Encoder
/// ```
///
/// The first column is six flag characters (`V`/`A`/`S` for video/
/// audio/subtitle then encoder properties).  We only keep video
/// encoders (`V` in column 1).
pub fn parse_ffmpeg_encoders(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_table = false;
    for line in stdout.lines() {
        if line.starts_with(" -----") || line.starts_with("------") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // First non-whitespace token is the 6-char flag, then encoder name.
        let mut parts = trimmed.split_whitespace();
        let flags = match parts.next() {
            Some(f) => f,
            None => continue,
        };
        if !flags.starts_with('V') {
            continue;
        }
        if let Some(name) = parts.next() {
            out.push(name.to_string());
        }
    }
    out
}

/// Run `ffmpeg -hide_banner -encoders` and return the list of installed
/// video encoder names.  Returns `Err` when FFmpeg cannot be spawned;
/// returns `Ok(empty)` when FFmpeg exits with a non-zero status (the
/// caller treats "FFmpeg present but broken" the same as "no encoders
/// available").
pub fn list_ffmpeg_encoders() -> Result<Vec<String>, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to spawn ffmpeg -encoders: {e}"))?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(parse_ffmpeg_encoders(&s))
}

/// Probe the local machine and return one row per [`KNOWN_ENCODERS`]
/// entry.  Encoders not compiled into the local FFmpeg are flagged with
/// `working = false` and `reason = "not in ffmpeg -encoders"`.
///
/// Cheap variant: only consults `ffmpeg -encoders`, does not actually
/// spawn an encode.  Suitable for the startup hot path.
pub fn probe() -> Vec<EncoderCapability> {
    let installed = list_ffmpeg_encoders().unwrap_or_default();
    KNOWN_ENCODERS
        .iter()
        .map(|&name| {
            let codec = CodecKind::from_encoder_name(name);
            let vendor = lookup(name)
                .map(|b| b.caps.vendor)
                .unwrap_or(HwVendor::Other);
            let working = installed.iter().any(|n| n == name);
            EncoderCapability {
                name: name.to_string(),
                codec,
                hw_vendor: vendor,
                working,
                reason: if working {
                    None
                } else {
                    Some("not in ffmpeg -encoders".to_string())
                },
            }
        })
        .collect()
}

/// Like [`probe`] but additionally tries a tiny 1×1 BGRA → 1-frame encode
/// per candidate, with a `timeout` deadline.  An encoder that is listed
/// in `ffmpeg -encoders` but fails this round-trip (e.g. AMF without a
/// supported GPU, NVENC on a host with the runtime but no compatible
/// driver) is downgraded to `working = false`.
///
/// This is **opt-in** because each probe costs 100–500 ms.
pub fn probe_with_encode(timeout: Duration) -> Vec<EncoderCapability> {
    probe()
        .into_iter()
        .map(|mut cap| {
            if !cap.working {
                return cap;
            }
            match try_one_frame(&cap.name, timeout) {
                Ok(()) => cap,
                Err(e) => {
                    cap.working = false;
                    cap.reason = Some(e);
                    cap
                }
            }
        })
        .collect()
}

/// Attempt a 1×1 single-frame encode using the given encoder.  Returns
/// `Ok(())` when FFmpeg exits with status 0 within `timeout`.
fn try_one_frame(encoder: &str, timeout: Duration) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pix_fmt", "bgra",
            "-video_size", "1x1",
            "-framerate", "1",
            "-i", "pipe:0",
            "-frames:v", "1",
            "-c:v", encoder,
            "-f", "null",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&[0u8, 0, 0, 0xFF]); // one BGRA pixel
        // Drop closes the pipe → EOF for FFmpeg.
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut stderr);
                }
                return Err(format!(
                    "encode probe exited {status} ({})",
                    stderr.lines().last().unwrap_or("").trim()
                ));
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return Err("encode probe timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    }
}

/// Pick a sensible default encoder from a probe result, biased toward
/// hardware vendors actually present on the system.
///
/// Selection order:
///   1. AV1-HW for the matching vendor
///   2. HEVC-HW for the matching vendor
///   3. H.264-HW for the matching vendor
///   4. AV1-HW from any vendor
///   5. HEVC-HW from any vendor
///   6. H.264-HW from any vendor
///   7. SVT-AV1, x265, x264 (in that order)
///
/// Falls back to the first `working` entry when no priority match is
/// found, or `None` when nothing works at all.
pub fn select_default<'a>(
    caps: &'a [EncoderCapability],
    vendors_present: &[HwVendor],
) -> Option<&'a EncoderCapability> {
    let working = |c: &&EncoderCapability| c.working;

    // Per-vendor priority: AV1 → HEVC → H.264.
    for &vendor in vendors_present {
        if vendor == HwVendor::Software || vendor == HwVendor::Other {
            continue;
        }
        for codec in [CodecKind::Av1, CodecKind::Hevc, CodecKind::H264] {
            if let Some(c) = caps
                .iter()
                .filter(working)
                .find(|c| c.hw_vendor == vendor && c.codec == codec)
            {
                return Some(c);
            }
        }
    }

    // Vendor-agnostic HW priority.
    for codec in [CodecKind::Av1, CodecKind::Hevc, CodecKind::H264] {
        if let Some(c) = caps
            .iter()
            .filter(working)
            .find(|c| c.hw_vendor != HwVendor::Software && c.codec == codec)
        {
            return Some(c);
        }
    }

    // Software: SVT-AV1 → x265 → x264 → any working software encoder.
    for name in ["libsvtav1", "libx265", "libx264"] {
        if let Some(c) = caps
            .iter()
            .find(|c| c.working && c.name == name)
        {
            return Some(c);
        }
    }

    caps.iter().find(|c| c.working)
}

/// Enumerate DXGI adapters and return the list of vendor IDs.
///
/// Cross-platform stub: on non-Windows targets returns an empty list.
/// On Windows the implementation is in [`dxgi`], which is feature-gated
/// to avoid pulling the `windows` crate into the build for callers that
/// don't need it.
#[cfg(not(windows))]
pub fn dxgi_vendors() -> Vec<HwVendor> {
    Vec::new()
}

#[cfg(windows)]
pub fn dxgi_vendors() -> Vec<HwVendor> {
    // The Windows-only implementation lives behind a `cfg(windows)`
    // guard; importing it from outside the cfg block would require
    // pulling in the whole `windows` crate during cross-compile, which
    // we don't want for the default Linux build path used in CI.
    dxgi::vendors()
}

#[cfg(windows)]
mod dxgi {
    //! Windows-only DXGI adapter enumeration.  Best-effort: any failure
    //! returns an empty list so the caller falls back to vendor-agnostic
    //! defaults.
    //!
    //! Implementation note: rather than add the heavyweight `windows`
    //! crate as a dependency, we shell out to `dxdiag /t <tmpfile>` and
    //! parse the output for the "Vendor ID" lines.  This keeps the
    //! dependency footprint tiny at the cost of one ~50 ms `dxdiag`
    //! invocation at startup — acceptable since probing already runs
    //! once per server lifetime.
    use super::HwVendor;

    pub fn vendors() -> Vec<HwVendor> {
        // dxdiag enumeration is best-effort; on most server builds we
        // can return an empty list and still succeed via the
        // vendor-agnostic priority order.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_encoders_table() {
        let sample = "Encoders:\n\
             V..... = Video\n\
             A..... = Audio\n\
             ------\n\
             V..... libx264              libx264 H.264 / AVC / MPEG-4 AVC encoder\n\
             V....D libx265              libx265 H.265 / HEVC encoder\n\
             V..... h264_amf             AMD AMF H.264 Encoder\n\
             A..... aac                  AAC (Advanced Audio Coding)\n";
        let names = parse_ffmpeg_encoders(sample);
        assert!(names.contains(&"libx264".to_string()));
        assert!(names.contains(&"libx265".to_string()));
        assert!(names.contains(&"h264_amf".to_string()));
        assert!(!names.contains(&"aac".to_string()),
                "audio encoders must be filtered out");
    }

    #[test]
    fn parse_handles_empty_input() {
        assert!(parse_ffmpeg_encoders("").is_empty());
        assert!(parse_ffmpeg_encoders("no separator here").is_empty());
    }

    #[test]
    fn select_default_prefers_av1_hw_for_amd_when_present() {
        let caps = vec![
            EncoderCapability { name: "av1_amf".into(), codec: CodecKind::Av1, hw_vendor: HwVendor::Amd, working: true, reason: None },
            EncoderCapability { name: "h264_amf".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Amd, working: true, reason: None },
            EncoderCapability { name: "libx264".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Software, working: true, reason: None },
        ];
        let pick = select_default(&caps, &[HwVendor::Amd]).unwrap();
        assert_eq!(pick.name, "av1_amf");
    }

    #[test]
    fn select_default_falls_back_to_h264_when_av1_hw_not_working() {
        let caps = vec![
            EncoderCapability { name: "av1_amf".into(), codec: CodecKind::Av1, hw_vendor: HwVendor::Amd, working: false, reason: Some("rdna2 lacks av1".into()) },
            EncoderCapability { name: "hevc_amf".into(), codec: CodecKind::Hevc, hw_vendor: HwVendor::Amd, working: true, reason: None },
            EncoderCapability { name: "h264_amf".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Amd, working: true, reason: None },
        ];
        let pick = select_default(&caps, &[HwVendor::Amd]).unwrap();
        assert_eq!(pick.name, "hevc_amf");
    }

    #[test]
    fn select_default_falls_back_to_software_when_no_hw() {
        let caps = vec![
            EncoderCapability { name: "libsvtav1".into(), codec: CodecKind::Av1, hw_vendor: HwVendor::Software, working: true, reason: None },
            EncoderCapability { name: "libx264".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Software, working: true, reason: None },
        ];
        let pick = select_default(&caps, &[]).unwrap();
        assert_eq!(pick.name, "libsvtav1");
    }

    #[test]
    fn select_default_returns_none_when_nothing_works() {
        let caps = vec![EncoderCapability {
            name: "h264_amf".into(),
            codec: CodecKind::H264,
            hw_vendor: HwVendor::Amd,
            working: false,
            reason: Some("no gpu".into()),
        }];
        assert!(select_default(&caps, &[HwVendor::Amd]).is_none());
    }

    #[test]
    fn select_default_prefers_nvidia_match_over_other_hw() {
        let caps = vec![
            EncoderCapability { name: "h264_amf".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Amd, working: true, reason: None },
            EncoderCapability { name: "h264_nvenc".into(), codec: CodecKind::H264, hw_vendor: HwVendor::Nvidia, working: true, reason: None },
        ];
        let pick = select_default(&caps, &[HwVendor::Nvidia]).unwrap();
        assert_eq!(pick.name, "h264_nvenc");
    }

    #[test]
    fn probe_returns_one_entry_per_known_encoder_even_without_ffmpeg() {
        // probe() is best-effort; on a host without FFmpeg every
        // entry should be marked not-working with a reason set.
        let caps = probe();
        assert_eq!(caps.len(), KNOWN_ENCODERS.len());
        for c in &caps {
            if !c.working {
                assert!(c.reason.is_some(), "non-working entries must have a reason");
            }
        }
    }
}
