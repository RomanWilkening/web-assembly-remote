use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{net::SocketAddr, time::Instant};
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

use crate::auth::{self, AuthState};
use crate::capture::{self, ScreenCapture};
use crate::config::AuthConfig;
use crate::cursor;
use crate::encoder::{EncodedFrame, FfmpegEncoder};
use crate::input::InputSimulator;
use crate::audio;

pub struct ServerConfig {
    pub addr: SocketAddr,
    pub fps: u32,
    pub quality: u8,
    pub encoder: String,
    pub static_dir: String,
    pub auth: AuthConfig,
    pub audio_device: Option<String>,
}

#[derive(Clone)]
struct AppState {
    fps: u32,
    quality: u8,
    encoder: String,
    auth: AuthState,
    audio_device: Option<String>,
}

impl axum::extract::FromRef<AppState> for AuthState {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

pub async fn run(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let auth_state = AuthState::new(&cfg.auth);

    let state = AppState {
        fps: cfg.fps,
        quality: cfg.quality,
        encoder: cfg.encoder,
        auth: auth_state.clone(),
        audio_device: cfg.audio_device,
    };

    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/login", get(auth::login_page))
        .route("/api/login", post(auth::login_handler))
        .route("/api/logout", post(auth::logout_handler))
        .route("/api/session", get(auth::session_check))
        .fallback_service(ServeDir::new(&cfg.static_dir))
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::auth_middleware,
        ))
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

    // ── 0. Enumerate monitors and send MonitorList ─────────────
    let monitors = tokio::task::spawn_blocking(capture::enumerate_monitors)
        .await
        .unwrap_or_default();

    if !monitors.is_empty() {
        let monitor_msg = protocol::ServerMessage::MonitorList {
            monitors: monitors.clone(),
        };
        log::info!("Sending MonitorList: {} monitor(s)", monitors.len());
        if ws_tx
            .send(Message::Binary(monitor_msg.encode().into()))
            .await
            .is_err()
        {
            log::error!("Failed to send MonitorList – client disconnected");
            return;
        }
    }

    // ── 0b. Enumerate audio devices and send AudioDeviceList ──
    let audio_devices = tokio::task::spawn_blocking(audio::enumerate_audio_devices)
        .await
        .unwrap_or_default();

    // Limit to 255 devices (u8 count in the protocol).
    let audio_devices = if audio_devices.len() > 255 {
        log::warn!(
            "Found {} audio devices, limiting to 255 in the protocol",
            audio_devices.len()
        );
        audio_devices[..255].to_vec()
    } else {
        audio_devices
    };

    let audio_device_list: Vec<protocol::AudioDeviceInfo> = audio_devices
        .iter()
        .enumerate()
        .map(|(i, name)| protocol::AudioDeviceInfo {
            index: i as u8,
            name: name.clone(),
        })
        .collect();

    // Always send the list (even if empty – tells client no devices available).
    let audio_list_msg = protocol::ServerMessage::AudioDeviceList {
        devices: audio_device_list,
    };
    log::info!("Sending AudioDeviceList: {} device(s)", audio_devices.len());
    for (i, name) in audio_devices.iter().enumerate() {
        log::info!("  Audio device {i}: \"{name}\"");
    }
    if ws_tx
        .send(Message::Binary(audio_list_msg.encode().into()))
        .await
        .is_err()
    {
        log::error!("Failed to send AudioDeviceList – client disconnected");
        return;
    }

    // ── 1. Wait for ClientReady or SelectMonitor ──────────────
    let mut selected_monitor: usize = 0; // default to primary

    // Wait for the first client message (ClientReady or SelectMonitor)
    loop {
        match ws_rx.next().await {
            Some(Ok(Message::Binary(data))) => {
                match protocol::ClientMessage::decode(&data) {
                    Some(protocol::ClientMessage::ClientReady) => break,
                    Some(protocol::ClientMessage::SelectMonitor { index }) => {
                        selected_monitor = index as usize;
                        break;
                    }
                    _ => {}
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                log::info!("Client disconnected before ready");
                return;
            }
            _ => {}
        }
    }

    // ── 2. Start capture on selected monitor ──────────────────

    // Look up the selected monitor's geometry (virtual-desktop position).
    let monitor_info = monitors.iter().find(|m| m.index as usize == selected_monitor);

    let screen_dims = {
        let monitor_idx = selected_monitor;
        tokio::task::spawn_blocking(move || {
            let capture = ScreenCapture::new_for_display(monitor_idx)
                .or_else(|_| ScreenCapture::new())
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((capture.width() as u16, capture.height() as u16))
        })
        .await
    };

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

    // Monitor position in the virtual desktop (from enumeration).
    let mon_x = monitor_info.map(|m| m.x as i32).unwrap_or(0);
    let mon_y = monitor_info.map(|m| m.y as i32).unwrap_or(0);

    let info_msg = protocol::ServerMessage::ServerInfo {
        width: screen_w,
        height: screen_h,
        fps: state.fps as u8,
    };
    log::info!(
        "Sending ServerInfo: {}×{} @ {} fps (monitor {} at {}, {})",
        screen_w,
        screen_h,
        state.fps,
        selected_monitor,
        mon_x,
        mon_y
    );
    if ws_tx
        .send(Message::Binary(info_msg.encode().into()))
        .await
        .is_err()
    {
        log::error!("Failed to send ServerInfo – client disconnected");
        return;
    }

    // Channel: encoder → WebSocket sender (small buffer to avoid latency).
    let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(2);

    // Channel: WebSocket receiver → input handler.
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(64);

    // Channel: cursor info sender.
    let (cursor_tx, mut cursor_rx) = mpsc::channel::<protocol::ServerMessage>(4);

    // Channel: audio capture → WebSocket sender.
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);

    // Channel: audio device control (None = stop, Some(name) = start).
    let (audio_ctl_tx, mut audio_ctl_rx) = mpsc::channel::<Option<String>>(4);

    let fps = state.fps;
    let quality = state.quality;
    let encoder_name = state.encoder.clone();
    let monitor_idx = selected_monitor;

    // ── 3. Spawn the capture + encode pipeline (blocking thread) ──
    let cap_mon_x = mon_x;
    let cap_mon_y = mon_y;
    let cap_mon_w = screen_w as u32;
    let cap_mon_h = screen_h as u32;
    let capture_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_loop(
            fps,
            quality,
            &encoder_name,
            frame_tx,
            cursor_tx,
            monitor_idx,
            cap_mon_x,
            cap_mon_y,
            cap_mon_w,
            cap_mon_h,
        ) {
            log::error!("Capture loop error: {e}");
        }
    });

    // ── 3b. Audio control task ──
    // Manages starting/stopping the FFmpeg audio capture thread based on
    // client requests.  If a default audio device was configured, start it
    // immediately so existing behaviour is preserved.
    let default_audio = state.audio_device.clone();
    let audio_ctl_handle = {
        let atx = audio_tx;
        tokio::spawn(async move {
            let mut current_handle: Option<tokio::task::JoinHandle<()>> = None;

            // Helper: start a new audio capture for the given device.
            let start = |dev: String, tx: mpsc::Sender<Vec<u8>>| {
                tokio::task::spawn_blocking(move || {
                    audio::audio_capture_loop(&dev, tx);
                })
            };

            // Auto-start if a default device was configured.
            if let Some(ref dev) = default_audio {
                log::info!("Auto-starting audio capture for configured device: \"{dev}\"");
                current_handle = Some(start(dev.clone(), atx.clone()));
            }

            while let Some(cmd) = audio_ctl_rx.recv().await {
                // Stop current audio capture (if any).
                if let Some(h) = current_handle.take() {
                    h.abort();
                    let _ = h.await;
                }

                // Start new capture if a device was requested.
                if let Some(dev) = cmd {
                    log::info!("Starting audio capture for device: \"{dev}\"");
                    current_handle = Some(start(dev, atx.clone()));
                } else {
                    log::info!("Audio capture stopped by client");
                }
            }

            // Channel closed – stop any running capture.
            if let Some(h) = current_handle.take() {
                h.abort();
            }
        })
    };

    // ── 4. Spawn the input handler (blocking thread) ──
    let input_sim = InputSimulator::new(mon_x, mon_y, screen_w as u32, screen_h as u32);
    let input_handle = tokio::task::spawn_blocking(move || {
        while let Some(data) = input_rx.blocking_recv() {
            if let Some(msg) = protocol::ClientMessage::decode(&data) {
                match msg {
                    protocol::ClientMessage::SelectMonitor { .. } => {
                        // Monitor switch is handled by reconnecting.
                        // Client should disconnect and reconnect with new selection.
                        log::info!("Monitor switch requested – client should reconnect");
                    }
                    // SelectAudio is handled in the WS receiver, not here.
                    protocol::ClientMessage::SelectAudio { .. } => {}
                    other => input_sim.handle(other),
                }
            }
        }
    });

    // ── 5. WebSocket sender task (video frames + cursor info + audio) ──
    let send_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(frame) = frame_rx.recv() => {
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
                Some(cursor_msg) = cursor_rx.recv() => {
                    let bin = cursor_msg.encode();
                    if ws_tx.send(Message::Binary(bin.into())).await.is_err() {
                        break;
                    }
                }
                Some(audio_data) = audio_rx.recv() => {
                    let msg = protocol::ServerMessage::AudioData { data: audio_data };
                    let bin = msg.encode();
                    if ws_tx.send(Message::Binary(bin.into())).await.is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // ── 6. WebSocket receiver (runs on this task) ──
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                // Intercept audio device selection before forwarding to
                // the input handler, because it needs async handling.
                if let Some(protocol::ClientMessage::SelectAudio { index }) =
                    protocol::ClientMessage::decode(&data)
                {
                    let cmd = if index == 0xFF {
                        None
                    } else {
                        audio_devices.get(index as usize).cloned()
                    };
                    let _ = audio_ctl_tx.send(cmd).await;
                } else {
                    let _ = input_tx.try_send(data.to_vec());
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    log::info!("WebSocket client disconnected");
    drop(input_tx);
    drop(audio_ctl_tx);
    capture_handle.abort();
    audio_ctl_handle.abort();
    let _ = send_handle.await;
    let _ = input_handle.await;
}

/// Main capture → encode loop. Runs on a dedicated OS thread.
/// Also sends cursor position updates alongside video frames.
///
/// Cursor coordinates are converted from absolute virtual-desktop
/// space to positions relative to the captured monitor so the client
/// can overlay them correctly.
fn capture_loop(
    fps: u32,
    quality: u8,
    encoder_name: &str,
    frame_tx: mpsc::Sender<EncodedFrame>,
    cursor_tx: mpsc::Sender<protocol::ServerMessage>,
    monitor_index: usize,
    monitor_x: i32,
    monitor_y: i32,
    monitor_w: u32,
    monitor_h: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut capture = ScreenCapture::new_for_display(monitor_index)
        .or_else(|_| ScreenCapture::new())?;
    let w = capture.width();
    let h = capture.height();

    log::info!(
        "Capture initialized: {}×{} @ {} fps (monitor {} at {}, {})",
        w,
        h,
        fps,
        monitor_index,
        monitor_x,
        monitor_y
    );

    let mut encoder = FfmpegEncoder::new(w, h, fps, quality, encoder_name, frame_tx)?;

    let frame_interval = std::time::Duration::from_micros(1_000_000 / u64::from(fps));
    let boot = Instant::now();
    let mut frame_no: u64 = 0;
    let mut last_cursor = (0u16, 0u16, false);

    loop {
        let target = boot + frame_interval.mul_f64(frame_no as f64);
        let now = Instant::now();
        if now < target {
            std::thread::sleep(target - now);
        }

        let bgra = capture.capture_frame()?;
        encoder.send_frame(&bgra)?;

        // Send cursor position (only when it changes or every ~10 frames).
        let (abs_cx, abs_cy, visible) = cursor::get_cursor_position();
        let (rel_cx, rel_cy, show) = cursor_to_monitor_relative(
            abs_cx, abs_cy, visible, monitor_x, monitor_y, monitor_w, monitor_h,
        );

        if (rel_cx, rel_cy, show) != last_cursor || frame_no % 10 == 0 {
            last_cursor = (rel_cx, rel_cy, show);
            let cursor_msg = protocol::ServerMessage::CursorInfo {
                x: rel_cx,
                y: rel_cy,
                visible: show,
            };
            let _ = cursor_tx.try_send(cursor_msg);
        }

        frame_no += 1;
    }
}

/// Convert absolute virtual-desktop cursor coordinates to monitor-relative
/// coordinates, clamped to `[0, dimension)`.  Returns `(rel_x, rel_y, visible)`
/// where `visible` is false if the cursor is outside the monitor rectangle.
fn cursor_to_monitor_relative(
    abs_x: u16,
    abs_y: u16,
    visible: bool,
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
) -> (u16, u16, bool) {
    let on_monitor = (abs_x as i32) >= mon_x
        && (abs_x as i32) < mon_x + mon_w as i32
        && (abs_y as i32) >= mon_y
        && (abs_y as i32) < mon_y + mon_h as i32;

    let max_x = mon_w.saturating_sub(1);
    let max_y = mon_h.saturating_sub(1);
    let rel_x = ((abs_x as i32 - mon_x).max(0) as u32).min(max_x) as u16;
    let rel_y = ((abs_y as i32 - mon_y).max(0) as u32).min(max_y) as u16;

    (rel_x, rel_y, visible && on_monitor)
}

fn timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
