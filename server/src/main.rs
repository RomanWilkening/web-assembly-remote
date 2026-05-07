mod audio;
mod auth;
mod capture;
mod config;
mod cursor;
mod diagnostics;
mod encoder;
mod hw_probe;
mod input;
mod server;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "wasm-remote-server", about = "Low-latency remote desktop server")]
struct Args {
    /// Listen address
    #[arg(short, long, default_value = "0.0.0.0")]
    host: String,

    /// Listen port
    #[arg(short, long, default_value_t = 9090)]
    port: u16,

    /// Target frames per second.  When omitted, the value from
    /// `[encoder.profiles.<default_profile>]` (or 60) is used.
    #[arg(long)]
    fps: Option<u32>,

    /// Encoder quality (QP/CRF value, lower = better quality, 15-30 recommended).
    /// Ignored when `--bitrate-kbps` switches the encoder to CBR.
    #[arg(long)]
    quality: Option<u8>,

    /// Video encoder to use.  Examples:
    /// `h264_amf` / `hevc_amf` / `av1_amf` (AMD GPU),
    /// `h264_nvenc` / `hevc_nvenc` / `av1_nvenc` (NVIDIA GPU),
    /// `libx264` / `libx265` / `libsvtav1` (CPU fallback).
    /// When omitted and the config has `[encoder].auto_select = true`
    /// the server picks the best available encoder via hardware probing.
    #[arg(long)]
    encoder: Option<String>,

    /// Override the codec family used for access-unit splitting and the
    /// browser's `VideoDecoder` configuration.  Auto-detected from
    /// `--encoder` when omitted.  Accepted values: `h264`, `hevc`, `av1`.
    #[arg(long)]
    codec: Option<String>,

    /// Chroma sub-sampling (`420` for compatibility, `444` for sharper text).
    #[arg(long)]
    chroma: Option<String>,

    /// Number of slices per encoded frame (>= 1).  Slicing reduces the
    /// "wait for the whole frame to arrive" decode latency at the cost
    /// of slightly worse compression efficiency.  Honoured by H.264 /
    /// HEVC encoders; ignored by AV1.
    #[arg(long)]
    slices: Option<u32>,

    /// Switch from constant-quality (CQP/CRF) to CBR with a 1-frame
    /// VBV buffer.  Useful on bandwidth-limited links where stable
    /// glass-to-glass latency matters more than constant visual
    /// quality.  In kilobits per second.  Disabled when omitted.
    #[arg(long)]
    bitrate_kbps: Option<u32>,

    /// Encoder profile name from `[encoder.profiles.<name>]`.  When
    /// omitted, the value from `[encoder].default_profile` is used (if
    /// any).
    #[arg(long)]
    profile: Option<String>,

    /// Path to static web files (client build output)
    #[arg(long, default_value = "./static")]
    static_dir: String,

    /// Path to configuration file (TOML)
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Audio output device name for loopback capture (overrides config file).
    /// On Windows: WASAPI render endpoint name, e.g. "Speakers (Realtek …)" or "default"
    /// On Linux: PulseAudio source name, e.g. "default"
    /// If not set, audio devices are auto-discovered and the user can select in the browser.
    #[arg(long)]
    audio_device: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    // Load configuration file.
    let app_config = config::AppConfig::load(&args.config)?;
    log::info!("Configuration loaded from '{}'", args.config.display());

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;

    // ── Resolve effective encoder settings ────────────────────
    // CLI flag (Some) > profile value (Some) > hardcoded default.
    let profile = app_config.profile(args.profile.as_deref()).cloned();
    if let Some(ref name) = args.profile {
        if profile.is_none() {
            return Err(format!("--profile '{name}' not found in config").into());
        }
    }
    if let Some(ref p) = profile {
        log::info!("Using encoder profile (encoder={:?}, fps={:?}, quality={:?})",
            p.encoder, p.fps, p.quality);
    }

    let encoder_name = args
        .encoder
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.encoder.clone()))
        .unwrap_or_else(|| "h264_amf".to_string());

    let fps = args
        .fps
        .or_else(|| profile.as_ref().and_then(|p| p.fps))
        .unwrap_or(60);

    let quality = args
        .quality
        .or_else(|| profile.as_ref().and_then(|p| p.quality))
        .unwrap_or(20);

    let slices = args
        .slices
        .or_else(|| profile.as_ref().and_then(|p| p.slices))
        .unwrap_or(1);

    let bitrate_kbps = args
        .bitrate_kbps
        .or_else(|| profile.as_ref().and_then(|p| p.bitrate_kbps));

    let codec_str = args
        .codec
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.codec.clone()));

    let codec = match codec_str.as_deref() {
        None => encoder::CodecKind::from_encoder_name(&encoder_name),
        Some("h264") => encoder::CodecKind::H264,
        Some("hevc") | Some("h265") => encoder::CodecKind::Hevc,
        Some("av1") => encoder::CodecKind::Av1,
        Some(other) => {
            return Err(format!(
                "Unknown --codec '{other}' (expected: h264, hevc, av1)"
            )
            .into());
        }
    };

    let chroma_str = args
        .chroma
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.chroma.clone()))
        .unwrap_or_else(|| "420".to_string());

    let chroma = match chroma_str.as_str() {
        "420" | "yuv420" | "yuv420p" => encoder::Chroma::Yuv420,
        "444" | "yuv444" | "yuv444p" => encoder::Chroma::Yuv444,
        other => {
            return Err(format!(
                "Unknown --chroma '{other}' (expected: 420 or 444)"
            )
            .into());
        }
    };

    if slices == 0 {
        return Err("--slices must be >= 1".into());
    }

    log::info!("Starting remote desktop server on {}", addr);
    log::info!(
        "Encoder: {} (codec={:?}), FPS: {}, Quality: {}, Chroma: {:?}, Slices: {}, Bitrate: {}",
        encoder_name,
        codec,
        fps,
        quality,
        chroma,
        slices,
        bitrate_kbps
            .map(|b| format!("{b} kbps (CBR)"))
            .unwrap_or_else(|| "CQP/CRF".into()),
    );
    log::info!("Static files: {}", args.static_dir);

    // Determine audio device: CLI flag takes precedence over the legacy
    // top-level `audio_device` field, which in turn beats the new
    // `[audio].default_device` value.  All three can be empty.
    let audio_device = args
        .audio_device
        .or(app_config.audio_device.clone())
        .or_else(|| app_config.audio.default_device.clone())
        .filter(|s| !s.is_empty());

    if let Some(ref dev) = audio_device {
        log::info!("Audio loopback device: {dev}");
    } else {
        log::info!("Audio: auto-discovery (user selects in browser)");
    }

    let config = server::ServerConfig {
        addr,
        fps,
        quality,
        encoder: encoder_name,
        codec,
        chroma,
        slices,
        bitrate_kbps,
        static_dir: args.static_dir,
        auth: app_config.auth.clone(),
        audio_device,
        app_config: Arc::new(app_config),
    };

    server::run(config).await
}
