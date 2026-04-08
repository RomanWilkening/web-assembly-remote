mod audio;
mod auth;
mod capture;
mod config;
mod cursor;
mod encoder;
mod input;
mod server;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "wasm-remote-server", about = "Low-latency remote desktop server")]
struct Args {
    /// Listen address
    #[arg(short, long, default_value = "0.0.0.0")]
    host: String,

    /// Listen port
    #[arg(short, long, default_value_t = 9090)]
    port: u16,

    /// Target frames per second
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Encoder quality (QP value, lower = better quality, 15-30 recommended)
    #[arg(long, default_value_t = 20)]
    quality: u8,

    /// Video encoder to use (h264_amf for AMD GPU, libx264 for CPU fallback)
    #[arg(long, default_value = "h264_amf")]
    encoder: String,

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

    log::info!("Starting remote desktop server on {}", addr);
    log::info!("Encoder: {}, FPS: {}, Quality (QP): {}", args.encoder, args.fps, args.quality);
    log::info!("Static files: {}", args.static_dir);

    // Determine audio device: CLI flag takes precedence over config file.
    let audio_device = args
        .audio_device
        .or(app_config.audio_device)
        .filter(|s| !s.is_empty());

    if let Some(ref dev) = audio_device {
        log::info!("Audio loopback device: {dev}");
    } else {
        log::info!("Audio: auto-discovery (user selects in browser)");
    }

    let config = server::ServerConfig {
        addr,
        fps: args.fps,
        quality: args.quality,
        encoder: args.encoder,
        static_dir: args.static_dir,
        auth: app_config.auth,
        audio_device,
    };

    server::run(config).await
}
