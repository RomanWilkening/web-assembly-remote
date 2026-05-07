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
use std::{collections::VecDeque, net::SocketAddr, time::Instant};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tower_http::services::ServeDir;

use crate::auth::{self, AuthState};
use crate::capture::{self, ScreenCapture};
use crate::config::{AppConfig, AuthConfig};
use crate::cursor;
use crate::diagnostics::{self, Diagnostics, EncoderResponse, HealthResponse};
use crate::encoder::{Chroma, CodecKind, EncodedFrame, EncoderConfig, FfmpegEncoder};
use crate::hw_probe::{self, EncoderCapability};
use crate::input::InputSimulator;
use crate::audio;
use axum::Json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

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
    /// Full parsed `config.toml` for reading profile data and
    /// `[encoder]` settings at runtime.
    pub app_config: Arc<AppConfig>,
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
    app_config: Arc<AppConfig>,
    /// Hardware-probed encoder list, computed once at server startup.
    encoder_caps: Arc<Vec<EncoderCapability>>,
    /// Live diagnostic counters (Block E).
    diagnostics: Arc<Diagnostics>,
}

impl axum::extract::FromRef<AppState> for AuthState {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

/// Map [`crate::encoder::backends::HwVendor`] to the byte sent on the
/// wire in `EncoderList`.  Stable; matches the documentation in
/// [`protocol::EncoderInfo::hw_vendor`].
fn hw_vendor_to_id(v: crate::encoder::backends::HwVendor) -> u8 {
    use crate::encoder::backends::HwVendor::*;
    match v {
        Amd => 0,
        Nvidia => 1,
        Intel => 2,
        Microsoft => 3,
        Software => 4,
        Other => 5,
    }
}

// ── Diagnostic HTTP handlers (Block E) ─────────────────────────────

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let snap = state.diagnostics.snapshot();
    Json(HealthResponse {
        ok: true,
        uptime_secs: snap.uptime_secs,
        active_sessions: snap.active_sessions,
    })
}

async fn stats_handler(
    State(state): State<AppState>,
) -> Json<diagnostics::DiagnosticsSnapshot> {
    Json(state.diagnostics.snapshot())
}

async fn encoders_handler(
    State(state): State<AppState>,
) -> Json<Vec<EncoderResponse>> {
    let body: Vec<EncoderResponse> = state
        .encoder_caps
        .iter()
        .map(EncoderResponse::from_capability)
        .collect();
    Json(body)
}

/// RAII guard that decrements `Diagnostics::active_sessions` when the
/// websocket session ends (regardless of how — return, panic, or
/// awaited future cancellation).
struct SessionGuard {
    diagnostics: Arc<Diagnostics>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.diagnostics
            .active_sessions
            .fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn run(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let auth_state = AuthState::new(&cfg.auth);

    // Hardware probing — runs once at server startup.  Findings drive
    // both the `MSG_ENCODER_LIST` payload sent to every client and (when
    // `[encoder].auto_select = true`) the runtime default encoder
    // selection.
    let encoder_caps = tokio::task::spawn_blocking(hw_probe::probe)
        .await
        .unwrap_or_default();
    log::info!(
        "Hardware probe: {} known encoders, {} working",
        encoder_caps.len(),
        encoder_caps.iter().filter(|c| c.working).count(),
    );
    for c in &encoder_caps {
        if c.working {
            log::info!("  encoder available: {} ({:?}, vendor={:?})",
                c.name, c.codec, c.hw_vendor);
        }
    }

    let state = AppState {
        fps: cfg.fps,
        quality: cfg.quality,
        encoder: cfg.encoder.clone(),
        codec: cfg.codec,
        chroma: cfg.chroma,
        slices: cfg.slices,
        bitrate_kbps: cfg.bitrate_kbps,
        auth: auth_state.clone(),
        audio_device: cfg.audio_device,
        app_config: cfg.app_config,
        encoder_caps: Arc::new(encoder_caps),
        diagnostics: Diagnostics::new(cfg.encoder),
    };

    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/login", get(auth::login_page))
        .route("/api/login", post(auth::login_handler))
        .route("/api/logout", post(auth::logout_handler))
        .route("/api/session", get(auth::session_check))
        .route("/api/health", get(health_handler))
        .route("/api/stats", get(stats_handler))
        .route("/api/encoders", get(encoders_handler))
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

    // Bump live-session counters; decrement automatically when this
    // function returns (panic-safe via Drop).
    state.diagnostics.active_sessions.fetch_add(1, Ordering::Relaxed);
    state.diagnostics.total_sessions.fetch_add(1, Ordering::Relaxed);
    let _session_guard = SessionGuard {
        diagnostics: state.diagnostics.clone(),
    };

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

    // ── 0c. Send EncoderList (Block A) ─────────────────────────
    let active_encoder_name = state.encoder.clone();
    let encoder_list: Vec<protocol::EncoderInfo> = state
        .encoder_caps
        .iter()
        .filter(|c| c.working)
        .take(255)
        .enumerate()
        .map(|(i, c)| protocol::EncoderInfo {
            index: i as u8,
            name: c.name.clone(),
            codec: c.codec.protocol_id(),
            hw_vendor: hw_vendor_to_id(c.hw_vendor),
            active: c.name == active_encoder_name,
        })
        .collect();
    let encoder_list_msg = protocol::ServerMessage::EncoderList {
        encoders: encoder_list,
    };
    log::info!(
        "Sending EncoderList: {} working encoder(s)",
        state.encoder_caps.iter().filter(|c| c.working).count(),
    );
    if ws_tx
        .send(Message::Binary(encoder_list_msg.encode().into()))
        .await
        .is_err()
    {
        log::error!("Failed to send EncoderList – client disconnected");
        return;
    }

    // ── 0d. Send ProfileList ───────────────────────────────────
    let active_profile = state.app_config.encoder.default_profile.clone();
    let profile_list: Vec<protocol::ProfileInfo> = state
        .app_config
        .encoder
        .profiles
        .keys()
        .take(255)
        .enumerate()
        .map(|(i, name)| protocol::ProfileInfo {
            index: i as u8,
            name: name.clone(),
            active: active_profile.as_deref() == Some(name.as_str()),
        })
        .collect();
    let profile_list_msg = protocol::ServerMessage::ProfileList {
        profiles: profile_list,
    };
    log::info!(
        "Sending ProfileList: {} profile(s)",
        state.app_config.encoder.profiles.len(),
    );
    if ws_tx
        .send(Message::Binary(profile_list_msg.encode().into()))
        .await
        .is_err()
    {
        log::error!("Failed to send ProfileList – client disconnected");
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
                    // Encoder/profile switching at runtime requires a
                    // full encoder-pipeline restart (the FFmpeg process
                    // cannot reconfigure mid-stream for most backends).
                    // The client should disconnect+reconnect after sending
                    // the request, mirroring the SelectMonitor behaviour.
                    // We log the request so operators can correlate with
                    // any subsequent reconnect.
                    protocol::ClientMessage::SelectEncoder { index } => {
                        log::info!(
                            "SelectEncoder({index}) – client should reconnect to apply"
                        );
                    }
                    protocol::ClientMessage::SelectProfile { index } => {
                        log::info!(
                            "SelectProfile({index}) – client should reconnect to apply"
                        );
                    }
                    other => input_sim.handle(other),
                }
            }
        }
    });

    // ── 5. WebSocket sender task (video frames + cursor info + audio + ping) ──
    let diagnostics_for_sender = state.diagnostics.clone();
    let send_handle = tokio::spawn(async move {
        let diagnostics = diagnostics_for_sender;
        // Track frame count at the previous diag tick so we can compute
        // a rolling FPS (frames sent in the last 5 s) for `/api/stats`.
        let mut last_sent_frames: u64 = 0;
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
                    // `audio_data` is already wire-framed by the
                    // capture loop: byte 0 is `MSG_AUDIO_DATA`, bytes
                    // 1.. are the raw PCM payload.  Ship it directly,
                    // skipping the redundant
                    // `ServerMessage::AudioData::encode()` round-trip
                    // that would otherwise allocate a fresh ~7.7 kB
                    // `Vec<u8>` on every 20 ms tick.
                    debug_assert!(
                        audio_data.first().copied() == Some(protocol::MSG_AUDIO_DATA),
                        "audio chunk must be pre-framed with MSG_AUDIO_DATA tag",
                    );
                    if ws_tx.send(Message::Binary(audio_data)).await.is_err() {
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
                    let delta = sent_frames.saturating_sub(last_sent_frames);
                    last_sent_frames = sent_frames;
                    diagnostics
                        .current_fps
                        .store(delta / 5, Ordering::Relaxed);
                }
                Some(frame) = frame_rx.recv() => {
                    // ── IDR-aware drop policy ──────────────────────────
                    //
                    // We must NEVER drop a P-frame in isolation: P-frames
                    // reference previous frames, and a WebCodecs (or any
                    // standards-compliant) decoder fed a P-frame whose
                    // predecessor was dropped will silently produce no
                    // output until the next IDR.  An earlier "newest-wins
                    // delta coalescer" did exactly this and reduced the
                    // perceived frame rate to ~1 fps (the IDR cadence).
                    //
                    // The only safe drop is a *whole sub-GOP*: if a
                    // newer IDR is already queued behind us, every frame
                    // currently in front of that IDR is about to be
                    // invalidated by the IDR's full decoder reset, so
                    // skipping straight to the IDR is lossless and
                    // recovers latency.  We therefore peek ahead with
                    // `try_recv` and, *only* when we find a newer IDR,
                    // discard the queued head and ship the IDR first.
                    //
                    // Backpressure on a slow link is handled by the
                    // WebSocket sink itself — `ws_tx.send().await`
                    // suspends until the OS write buffer drains, which
                    // naturally rate-limits the encoder via the bounded
                    // `frame_rx` channel.
                    let mut to_send: VecDeque<EncodedFrame> = VecDeque::new();
                    to_send.push_back(frame);
                    // Drain whatever is already queued so we can spot a
                    // newer IDR without blocking.
                    while let Ok(next) = frame_rx.try_recv() {
                        if next.is_keyframe {
                            // Everything queued so far is about to be
                            // superseded by this IDR's decoder reset —
                            // safe to drop it all.
                            dropped_deltas += to_send.len() as u64;
                            diagnostics
                                .dropped_intermediate_deltas
                                .fetch_add(to_send.len() as u64, Ordering::Relaxed);
                            to_send.clear();
                        }
                        to_send.push_back(next);
                    }

                    while let Some(mut f) = to_send.pop_front() {
                        debug_assert!(f.data.len() >= EncodedFrame::HEADER_LEN);
                        let is_key = f.is_keyframe;
                        let ts = timestamp_us().to_le_bytes();
                        f.data[0] = protocol::MSG_VIDEO_FRAME;
                        f.data[1..9].copy_from_slice(&ts);
                        f.data[9] = u8::from(is_key);
                        let payload_len = f.data.len();
                        if ws_tx.send(Message::Binary(f.data)).await.is_err() {
                            // Inner `while let` would only break the
                            // drain loop, not the outer select loop —
                            // exit the whole sender task instead.
                            return;
                        }
                        sent_frames += 1;
                        diagnostics.frames_sent.fetch_add(1, Ordering::Relaxed);
                        diagnostics
                            .bytes_sent
                            .fetch_add(payload_len as u64, Ordering::Relaxed);
                        if is_key {
                            sent_keys += 1;
                        }
                        if sent_frames <= 2 {
                            log::info!(
                                "ws-sender: shipped frame #{} ({}, {} bytes incl. header)",
                                sent_frames,
                                if is_key { "key" } else { "delta" },
                                payload_len
                            );
                        } else if is_key {
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
