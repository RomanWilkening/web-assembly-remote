use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{net::SocketAddr, time::Instant};
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

use crate::capture::ScreenCapture;
use crate::encoder::{EncodedFrame, FfmpegEncoder};
use crate::input::InputSimulator;
use scrap::Display;

pub struct ServerConfig {
    pub addr: SocketAddr,
    pub fps: u32,
    pub quality: u8,
    pub encoder: String,
    pub static_dir: String,
}

#[derive(Clone)]
struct AppState {
    fps: u32,
    quality: u8,
    encoder: String,
}

pub async fn run(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        fps: cfg.fps,
        quality: cfg.quality,
        encoder: cfg.encoder,
    };

    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .fallback_service(ServeDir::new(&cfg.static_dir))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;

    // Enable TCP_NODELAY on accepted connections for lower latency.
    log::info!("Listening on http://{}", cfg.addr);
    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    log::info!("WebSocket client connected");

    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── 0. Query display dimensions and send ServerInfo ────────
    let screen_dims = tokio::task::spawn_blocking(|| {
        let display = Display::primary().map_err(|e| e.to_string())?;
        Ok::<_, String>((display.width() as u16, display.height() as u16))
    })
    .await;

    let (screen_w, screen_h) = match screen_dims {
        Ok(Ok(dims)) => dims,
        Ok(Err(e)) => {
            log::error!("Failed to query display: {e}");
            return;
        }
        Err(e) => {
            log::error!("Task join error: {e}");
            return;
        }
    };

    let info_msg = protocol::ServerMessage::ServerInfo {
        width: screen_w,
        height: screen_h,
        fps: state.fps as u8,
    };
    log::info!("Sending ServerInfo: {}×{} @ {} fps", screen_w, screen_h, state.fps);
    if ws_tx.send(Message::Binary(info_msg.encode().into())).await.is_err() {
        log::error!("Failed to send ServerInfo – client disconnected");
        return;
    }

    // Channel: encoder → WebSocket sender (small buffer to avoid latency).
    let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(2);

    // Channel: WebSocket receiver → input handler.
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(64);

    let fps = state.fps;
    let quality = state.quality;
    let encoder_name = state.encoder.clone();

    // ── 1. Spawn the capture + encode pipeline (blocking thread) ──
    let capture_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_loop(fps, quality, &encoder_name, frame_tx) {
            log::error!("Capture loop error: {e}");
        }
    });

    // ── 2. Spawn the input handler (blocking thread) ──
    let input_handle = tokio::task::spawn_blocking(move || {
        while let Some(data) = input_rx.blocking_recv() {
            if let Some(msg) = protocol::ClientMessage::decode(&data) {
                InputSimulator::handle(msg);
            }
        }
    });

    // ── 3. WebSocket sender task ──
    let send_handle = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let ts = timestamp_us();
            let msg = protocol::ServerMessage::VideoFrame {
                timestamp_us: ts,
                is_keyframe: frame.is_keyframe,
                data: frame.data,
            };
            let bin = msg.encode();
            if ws_tx.send(Message::Binary(bin.into())).await.is_err() {
                break;
            }
        }
    });

    // ── 4. WebSocket receiver (runs on this task) ──
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                let _ = input_tx.try_send(data.to_vec());
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    log::info!("WebSocket client disconnected");
    drop(input_tx);
    capture_handle.abort();
    let _ = send_handle.await;
    let _ = input_handle.await;
}

/// Main capture → encode loop. Runs on a dedicated OS thread.
fn capture_loop(
    fps: u32,
    quality: u8,
    encoder_name: &str,
    frame_tx: mpsc::Sender<EncodedFrame>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut capture = ScreenCapture::new()?;
    let w = capture.width();
    let h = capture.height();

    log::info!("Capture initialized: {}×{} @ {} fps", w, h, fps);

    let mut encoder = FfmpegEncoder::new(w, h, fps, quality, encoder_name, frame_tx)?;

    let frame_interval = std::time::Duration::from_micros(1_000_000 / u64::from(fps));
    let boot = Instant::now();
    let mut frame_no: u64 = 0;

    loop {
        let target = boot + frame_interval.mul_f64(frame_no as f64);
        let now = Instant::now();
        if now < target {
            std::thread::sleep(target - now);
        }

        let bgra = capture.capture_frame()?;
        encoder.send_frame(&bgra)?;

        frame_no += 1;
    }
}

fn timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
