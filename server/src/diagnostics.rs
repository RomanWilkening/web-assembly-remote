//! Lightweight diagnostic HTTP endpoints (Block E).
//!
//! All routes are mounted under `/api/` and inherit the existing
//! cookie-based auth middleware in [`crate::auth`] — no separate
//! authentication is added here.
//!
//! Endpoints:
//!
//! * `GET /api/health`   – cheap liveness probe (uptime, active sessions).
//! * `GET /api/stats`    – counters published by the live encoder
//!                         (frames in/out, bytes, restarts, current FPS).
//! * `GET /api/encoders` – the hardware-probe result; same data the
//!                         client gets via `MSG_ENCODER_LIST`.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Shared diagnostic counters used by the running server.
///
/// Held in an [`Arc`] so the encoder pipeline, the WebSocket handler,
/// and the HTTP layer can update / read it concurrently.
#[derive(Debug)]
pub struct Diagnostics {
    /// Server start time (used for uptime computation).
    pub start: Instant,
    /// Number of WebSocket sessions currently open.
    pub active_sessions: AtomicUsize,
    /// Total WebSocket sessions accepted since boot.
    pub total_sessions: AtomicU64,
    /// Number of times the encoder pipeline has been (re)started.
    pub encoder_restarts: AtomicU64,
    /// Most-recently chosen encoder name.
    pub current_encoder: parking_lot_compat::Mutex<String>,
    /// Most-recently observed FPS (frames sent in the last 1-second window).
    pub current_fps: AtomicU64,
    /// Total video frames sent on the wire.
    pub frames_sent: AtomicU64,
    /// Total bytes sent on the wire (video + audio + cursor).
    pub bytes_sent: AtomicU64,
    /// Total intermediate delta-frames dropped by the send-side coalescer.
    pub dropped_intermediate_deltas: AtomicU64,
}

impl Diagnostics {
    pub fn new(initial_encoder: String) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            active_sessions: AtomicUsize::new(0),
            total_sessions: AtomicU64::new(0),
            encoder_restarts: AtomicU64::new(0),
            current_encoder: parking_lot_compat::Mutex::new(initial_encoder),
            current_fps: AtomicU64::new(0),
            frames_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            dropped_intermediate_deltas: AtomicU64::new(0),
        })
    }

    /// Snapshot of the counters; cheap (atomic loads only).
    pub fn snapshot(&self) -> DiagnosticsSnapshot {
        DiagnosticsSnapshot {
            uptime_secs: self.start.elapsed().as_secs(),
            active_sessions: self.active_sessions.load(Ordering::Relaxed),
            total_sessions: self.total_sessions.load(Ordering::Relaxed),
            encoder_restarts: self.encoder_restarts.load(Ordering::Relaxed),
            current_encoder: self.current_encoder.lock().clone(),
            current_fps: self.current_fps.load(Ordering::Relaxed),
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            dropped_intermediate_deltas: self
                .dropped_intermediate_deltas
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct DiagnosticsSnapshot {
    pub uptime_secs: u64,
    pub active_sessions: usize,
    pub total_sessions: u64,
    pub encoder_restarts: u64,
    pub current_encoder: String,
    pub current_fps: u64,
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub dropped_intermediate_deltas: u64,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub uptime_secs: u64,
    pub active_sessions: usize,
}

#[derive(Serialize)]
pub struct EncoderResponse {
    pub name: String,
    pub codec: String,
    pub hw_vendor: String,
    pub working: bool,
    pub reason: Option<String>,
}

impl EncoderResponse {
    pub fn from_capability(c: &crate::hw_probe::EncoderCapability) -> Self {
        Self {
            name: c.name.clone(),
            codec: format!("{:?}", c.codec),
            hw_vendor: format!("{:?}", c.hw_vendor),
            working: c.working,
            reason: c.reason.clone(),
        }
    }
}

/// Tiny `parking_lot`-style mutex shim built on `std::sync::Mutex` so we
/// don't pull in a new dependency.  Encapsulated here so the rest of the
/// module can read like canonical observability code.
mod parking_lot_compat {
    pub struct Mutex<T>(std::sync::Mutex<T>);

    impl<T> std::fmt::Debug for Mutex<T>
    where
        T: std::fmt::Debug,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_tuple("Mutex").field(&self.0).finish()
        }
    }

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_increments() {
        let d = Diagnostics::new("h264_amf".into());
        d.active_sessions.fetch_add(2, Ordering::Relaxed);
        d.total_sessions.fetch_add(7, Ordering::Relaxed);
        d.frames_sent.fetch_add(120, Ordering::Relaxed);
        d.encoder_restarts.fetch_add(1, Ordering::Relaxed);
        let s = d.snapshot();
        assert_eq!(s.active_sessions, 2);
        assert_eq!(s.total_sessions, 7);
        assert_eq!(s.frames_sent, 120);
        assert_eq!(s.encoder_restarts, 1);
        assert_eq!(s.current_encoder, "h264_amf");
    }

    #[test]
    fn encoder_response_serialises_capability_fields() {
        use crate::encoder::backends::HwVendor;
        use crate::encoder::CodecKind;
        let cap = crate::hw_probe::EncoderCapability {
            name: "libsvtav1".into(),
            codec: CodecKind::Av1,
            hw_vendor: HwVendor::Software,
            working: true,
            reason: None,
        };
        let resp = EncoderResponse::from_capability(&cap);
        let json = serde_json::to_string(&resp).expect("serialises");
        assert!(json.contains("\"name\":\"libsvtav1\""));
        assert!(json.contains("\"codec\":\"Av1\""));
        assert!(json.contains("\"hw_vendor\":\"Software\""));
        assert!(json.contains("\"working\":true"));
    }

    #[test]
    fn health_response_serialises() {
        let r = HealthResponse {
            ok: true,
            uptime_secs: 42,
            active_sessions: 3,
        };
        let json = serde_json::to_string(&r).expect("serialises");
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"uptime_secs\":42"));
        assert!(json.contains("\"active_sessions\":3"));
    }
}
