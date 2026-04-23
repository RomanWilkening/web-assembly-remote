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
use tokio::time::{self, Duration};
use tower_http::services::ServeDir;

use crate::auth::{self, AuthState};
use crate::capture::{self, ScreenCapture};
use crate::config::AuthConfig;
use crate::cursor;
use crate::encoder::{Chroma, CodecKind, EncodedFrame, EncoderConfig, FfmpegEncoder};
use crate::input::InputSimulator;
use crate::audio;

pub struct ServerConfig {
    pub addr: SocketAddr,
    pub fps: u32,
    pub quality: u8,
    pub encoder: String,
    pub codec: CodecKind,
    pub chroma: Chroma,
    pub slices: u32,
    pub bitrate_kbps: Option<u32>,
    pub static_dir: String,
    pub auth: AuthConfig,
    pub audio_device: Option<String>,
}

#[derive(Clone)]
struct AppState {
    fps: u32,
    quality: u8,
    encoder: String,
    codec: CodecKind,
    chroma: Chroma,
    slices: u32,
    bitrate_kbps: Option<u32>,
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
        codec: cfg.codec,
        chroma: cfg.chroma,
        slices: cfg.slices,
        bitrate_kbps: cfg.bitrate_kbps,
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

    // Enable TCP_NODELAY on every accepted connection.  Without this,
    // Nagle's algorithm can hold back small frames (cursor updates,
    // pongs, audio chunks, small delta-frames) for up to 40 ms — a
    // direct hit on the interactive latency path.
    log::info!("Listening on http://{}", cfg.addr);
    axum::serve(listener, app.into_make_service())
        .tcp_nodelay(true)
        .await?;

    Ok(())
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Note: WebSocket per-message-deflate (`permessage-deflate`) is
    // intentionally **not** negotiated here.  Our payload mix is:
    //   * Video — already H.264-compressed; deflate makes it bigger.
    //   * Audio — small interleaved-PCM chunks, ~7.5 kB each, also
    //     not compressible enough to justify the per-chunk CPU cost
    //     on both ends.
    //   * Cursor / pong / control — too small for deflate to help.
    // axum 0.7's WebSocketUpgrade does not enable compression by
    // default, so this is a documentation-only reminder: do not
    // turn it on without re-benchmarking.
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
        codec: state.codec.protocol_id(),
    };
    log::info!(
        "Sending ServerInfo: {}×{} @ {} fps, codec={:?} (id {}) (monitor {} at {}, {})",
        screen_w,
        screen_h,
        state.fps,
        state.codec,
        state.codec.protocol_id(),
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

    // Channel: encoder → WebSocket sender.  Capacity 4 is large enough to
    // absorb a brief WebSocket-write stall (e.g. a slow Wi-Fi burst) so
    // the encoder thread doesn't block on `send()`, but small enough that
    // the *send-side* coalescer (see below) can keep latency low by
    // dropping intermediate delta frames whenever the link is the
    // bottleneck.
    let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(4);

    // Channel: WebSocket receiver → input handler.
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(64);

    // Channel: cursor info sender.
    let (cursor_tx, mut cursor_rx) = mpsc::channel::<protocol::ServerMessage>(4);

    // Channel: audio capture → WebSocket sender.
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);

    // Channel: audio device control (None = stop, Some(name) = start).
    let (audio_ctl_tx, mut audio_ctl_rx) = mpsc::channel::<Option<String>>(4);

    // Channel: ping replies (filled by the receiver, drained by the sender).
    let (pong_tx, mut pong_rx) = mpsc::channel::<u64>(8);

    let fps = state.fps;
    let quality = state.quality;
    let encoder_name = state.encoder.clone();
    let codec = state.codec;
    let chroma = state.chroma;
    let slices = state.slices;
    let bitrate_kbps = state.bitrate_kbps;
    let monitor_idx = selected_monitor;

    // ── 3. Spawn the capture + encode pipeline (blocking thread) ──
    let cap_mon_x = mon_x;
    let cap_mon_y = mon_y;
    let cap_mon_w = screen_w as u32;
    let cap_mon_h = screen_h as u32;
    let capture_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = capture_loop(
            CaptureLoopArgs {
                fps,
                quality,
                encoder_name: &encoder_name,
                codec,
                chroma,
                slices,
                bitrate_kbps,
                frame_tx,
                monitor_index: monitor_idx,
                monitor_x: cap_mon_x,
                monitor_y: cap_mon_y,
                monitor_w: cap_mon_w,
                monitor_h: cap_mon_h,
            },
        ) {
            log::error!("Capture loop error: {e}");
        }
    });

    // ── 3a. Cursor polling task (decoupled from capture FPS) ──
    //
    // Polling the cursor on its own 120 Hz tick (instead of once per
    // captured video frame) gives the user a responsive cursor even
    // at lower encoder FPS, *and* removes the redundant "send every
    // 10 frames regardless of change" path: the client only sees
    // updates when the cursor actually moves or its visibility
    // changes, so total bandwidth doesn't go up.
    let cursor_mon_x = mon_x;
    let cursor_mon_y = mon_y;
    let cursor_mon_w = screen_w as u32;
    let cursor_mon_h = screen_h as u32;
    let cursor_handle = tokio::task::spawn_blocking(move || {
        let interval = std::time::Duration::from_micros(1_000_000 / 120);
        let mut last_sent = (u16::MAX, u16::MAX, false);
        loop {
            let start = Instant::now();
            let (abs_cx, abs_cy, visible) = cursor::get_cursor_position();
            let (rel_cx, rel_cy, show) = cursor_to_monitor_relative(
                abs_cx,
                abs_cy,
                visible,
                cursor_mon_x,
                cursor_mon_y,
                cursor_mon_w,
                cursor_mon_h,
            );
            let next = (rel_cx, rel_cy, show);
            if next != last_sent {
                last_sent = next;
                let msg = protocol::ServerMessage::CursorInfo {
                    x: rel_cx,
                    y: rel_cy,
                    visible: show,
                };
                if cursor_tx.blocking_send(msg).is_err() {
                    // WebSocket closed.
                    break;
                }
            }
            // Sleep the remainder of the 120 Hz tick.  Catch-up if we
            // overran (e.g. cursor_to_monitor_relative did some work).
            if let Some(rem) = interval.checked_sub(start.elapsed()) {
                std::thread::sleep(rem);
            }
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

    // ── 5. WebSocket sender task (video frames + cursor info + audio + ping) ──
    let send_handle = tokio::spawn(async move {
        // Periodic WebSocket pings keep SSL-inspecting proxies (e.g.
        // Netskope) from buffering data indefinitely.  The small control
        // frame forces the proxy to flush its write pipeline.
        let mut ping_interval = time::interval(Duration::from_secs(5));
        ping_interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so the first ping fires after 5 s.
        ping_interval.tick().await;

        // ── Diagnostic counters (paired with `encoder-reader` log) ──
        // A summary every 5 s lets us correlate "frames produced by the
        // encoder" with "frames actually shipped to the client", which
        // in turn rules out (or in) starvation between the encoder
        // reader thread and the WebSocket writer.
        let mut sent_frames: u64 = 0;
        let mut sent_keys: u64 = 0;
        let mut dropped_deltas: u64 = 0;
        let mut diag_interval = time::interval(Duration::from_secs(5));
        diag_interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        diag_interval.tick().await;

        loop {
            tokio::select! {
                // `biased;` makes the select arms be polled in source
                // order on every iteration.  We poll Pong first so RTT
                // measurements are not delayed behind a large pending
                // I-frame, then the rest of the small/control messages
                // before the (potentially large) video stream.
                biased;

                Some(client_ts_us) = pong_rx.recv() => {
                    let bin = protocol::ServerMessage::Pong { client_ts_us }.encode();
                    if ws_tx.send(Message::Binary(bin)).await.is_err() {
                        break;
                    }
                }
                Some(cursor_msg) = cursor_rx.recv() => {
                    let bin = cursor_msg.encode();
                    if ws_tx.send(Message::Binary(bin)).await.is_err() {
                        break;
                    }
                }
                Some(audio_data) = audio_rx.recv() => {
                    let msg = protocol::ServerMessage::AudioData { data: audio_data };
                    let bin = msg.encode();
                    if ws_tx.send(Message::Binary(bin)).await.is_err() {
                        break;
                    }
                }
                _ = ping_interval.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
                _ = diag_interval.tick() => {
                    log::info!(
                        "ws-sender: sent_frames={} (keys={}, dropped_intermediate_deltas={})",
                        sent_frames,
                        sent_keys,
                        dropped_deltas
                    );
                }
                Some(frame) = frame_rx.recv() => {
                    // ── Send-side newest-wins coalescing ────────────────
                    //
                    // If multiple frames piled up in the channel while the
                    // previous WebSocket write was in flight (typical on a
                    // congested Wi-Fi or after a TCP retransmit) we want
                    // to discard the *intermediate* delta frames and only
                    // ship the most recent picture — the user only sees
                    // the latest frame anyway, and dropping the stale
                    // ones recovers latency immediately.
                    //
                    // Key-frames are NEVER dropped: losing one would leave
                    // the decoder unable to recover until the next IDR
                    // (~2 s with the current GOP), which is far worse
                    // than carrying one extra frame of delay.
                    //
                    // Pending state is tracked as `Option<EncodedFrame>`
                    // so we can keep the latest of each kind without an
                    // intermediate placeholder allocation.
                    let mut pending_key: Option<EncodedFrame> = None;
                    let mut pending_delta: Option<EncodedFrame> = None;
                    if frame.is_keyframe {
                        pending_key = Some(frame);
                    } else {
                        pending_delta = Some(frame);
                    }
                    while let Ok(next) = frame_rx.try_recv() {
                        if next.is_keyframe {
                            // Newer key supersedes any earlier key.
                            if pending_key.is_some() {
                                dropped_deltas += 1; // count superseded frame
                            }
                            pending_key = Some(next);
                        } else {
                            // Newer delta supersedes any earlier delta.
                            if pending_delta.is_some() {
                                dropped_deltas += 1;
                            }
                            pending_delta = Some(next);
                        }
                    }

                    // Send the (possibly skipped-ahead) key frame first
                    // so the decoder can consume it before the latest
                    // delta.
                    if let Some(mut kf) = pending_key {
                        debug_assert!(kf.data.len() >= EncodedFrame::HEADER_LEN);
                        let ts = timestamp_us().to_le_bytes();
                        kf.data[0] = protocol::MSG_VIDEO_FRAME;
                        kf.data[1..9].copy_from_slice(&ts);
                        kf.data[9] = u8::from(kf.is_keyframe);
                        let payload_len = kf.data.len();
                        if ws_tx.send(Message::Binary(kf.data)).await.is_err() {
                            break;
                        }
                        sent_frames += 1;
                        sent_keys += 1;
                        if sent_frames <= 2 {
                            log::info!(
                                "ws-sender: shipped frame #{} (key, {} bytes incl. header)",
                                sent_frames,
                                payload_len
                            );
                        } else {
                            // Mirror the encoder-reader's per-keyframe log
                            // (see `encoder_reader_loop`) so an operator can
                            // correlate "encoder-reader: emitted KEY frame
                            // #N" with the same frame leaving the WebSocket.
                            log::info!(
                                "ws-sender: shipped KEY frame #{} (#{} key, {} bytes incl. header)",
                                sent_frames,
                                sent_keys,
                                payload_len
                            );
                        }
                    }

                    if let Some(mut df) = pending_delta {
                        debug_assert!(df.data.len() >= EncodedFrame::HEADER_LEN);
                        let ts = timestamp_us().to_le_bytes();
                        df.data[0] = protocol::MSG_VIDEO_FRAME;
                        df.data[1..9].copy_from_slice(&ts);
                        df.data[9] = u8::from(df.is_keyframe);
                        let payload_len = df.data.len();
                        if ws_tx.send(Message::Binary(df.data)).await.is_err() {
                            break;
                        }
                        sent_frames += 1;
                        if sent_frames <= 2 {
                            log::info!(
                                "ws-sender: shipped frame #{} (delta, {} bytes incl. header)",
                                sent_frames,
                                payload_len
                            );
                        }
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
                // Intercept messages that need special async handling
                // (audio device selection, ping/pong) before forwarding to
                // the input handler.
                match protocol::ClientMessage::decode(&data) {
                    Some(protocol::ClientMessage::SelectAudio { index }) => {
                        let cmd = if index == 0xFF {
                            None
                        } else {
                            audio_devices.get(index as usize).cloned()
                        };
                        let _ = audio_ctl_tx.send(cmd).await;
                    }
                    Some(protocol::ClientMessage::Ping { client_ts_us }) => {
                        // Echo back as Pong so the client can compute RTT
                        // against its own clock (no NTP sync required).
                        let _ = pong_tx.try_send(client_ts_us);
                    }
                    _ => {
                        let _ = input_tx.try_send(data.to_vec());
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    log::info!("WebSocket client disconnected");
    drop(input_tx);
    drop(audio_ctl_tx);
    drop(pong_tx);
    capture_handle.abort();
    cursor_handle.abort();
    audio_ctl_handle.abort();
    let _ = send_handle.await;
    let _ = input_handle.await;
}

/// Bundled arguments for [`capture_loop`].  Avoids growing the
/// function signature past sanity as we add encoder options.
struct CaptureLoopArgs<'a> {
    fps: u32,
    quality: u8,
    encoder_name: &'a str,
    codec: CodecKind,
    chroma: Chroma,
    slices: u32,
    bitrate_kbps: Option<u32>,
    frame_tx: mpsc::Sender<EncodedFrame>,
    monitor_index: usize,
    monitor_x: i32,
    monitor_y: i32,
    monitor_w: u32,
    monitor_h: u32,
}

/// Main capture → encode loop. Runs on a dedicated OS thread.
///
/// Cursor polling has been split off into its own task (see
/// `cursor_handle` in [`run`]) so cursor latency is no longer coupled
/// to the encoder FPS.
fn capture_loop(args: CaptureLoopArgs<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let mut capture = ScreenCapture::new_for_display(args.monitor_index)
        .or_else(|_| ScreenCapture::new())?;
    let w = capture.width();
    let h = capture.height();

    log::info!(
        "Capture initialized: {}×{} @ {} fps (monitor {} at {}, {})",
        w,
        h,
        args.fps,
        args.monitor_index,
        args.monitor_x,
        args.monitor_y
    );
    // monitor_w/h are passed in for symmetry with the cursor task; they
    // are not used directly here because the captured display already
    // reports its own dimensions.
    let _ = (args.monitor_w, args.monitor_h);

    let cfg = EncoderConfig {
        width: w,
        height: h,
        fps: args.fps,
        quality: args.quality,
        encoder_name: args.encoder_name.to_string(),
        codec: args.codec,
        chroma: args.chroma,
        slices: args.slices,
        bitrate_kbps: args.bitrate_kbps,
    };
    let mut encoder = FfmpegEncoder::new(cfg, args.frame_tx)?;

    let frame_interval = std::time::Duration::from_micros(1_000_000 / u64::from(args.fps));
    let boot = Instant::now();
    let mut frame_no: u64 = 0;

    loop {
        let target = boot + frame_interval.mul_f64(frame_no as f64);
        let now = Instant::now();
        if now < target {
            std::thread::sleep(target - now);
        }

        let bgra = capture.capture_frame()?;
        encoder.send_frame(bgra)?;

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
