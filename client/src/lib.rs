use wasm_bindgen::prelude::*;

// Re-export protocol constants so JS can reference them if needed.
pub use protocol::{
    MSG_CLIENT_READY, MSG_KEY_EVENT, MSG_MOUSE_BUTTON, MSG_MOUSE_MOVE,
    MSG_MOUSE_SCROLL, MSG_SERVER_INFO, MSG_VIDEO_FRAME,
};

// ---------------------------------------------------------------------------
// Encode helpers – called from JavaScript to build binary messages.
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub fn encode_client_ready() -> Vec<u8> {
    protocol::ClientMessage::ClientReady.encode()
}

#[wasm_bindgen]
pub fn encode_mouse_move(x: u16, y: u16) -> Vec<u8> {
    protocol::ClientMessage::MouseMove { x, y }.encode()
}

#[wasm_bindgen]
pub fn encode_mouse_button(button: u8, pressed: bool, x: u16, y: u16) -> Vec<u8> {
    protocol::ClientMessage::MouseButton { button, pressed, x, y }.encode()
}

#[wasm_bindgen]
pub fn encode_mouse_scroll(delta_x: i16, delta_y: i16) -> Vec<u8> {
    protocol::ClientMessage::MouseScroll { delta_x, delta_y }.encode()
}

#[wasm_bindgen]
pub fn encode_key_event(key_code: u16, pressed: bool) -> Vec<u8> {
    protocol::ClientMessage::KeyEvent { key_code, pressed }.encode()
}

// ---------------------------------------------------------------------------
// Decode helpers – called from JavaScript to parse incoming server messages.
// ---------------------------------------------------------------------------

/// Decode the first byte of a server message to determine its type.
/// Returns the MSG_* constant, or 0 if invalid.
#[wasm_bindgen]
pub fn message_type(data: &[u8]) -> u8 {
    data.first().copied().unwrap_or(0)
}

/// For a VideoFrame message, extract the 8-byte timestamp (microseconds).
#[wasm_bindgen]
pub fn video_frame_timestamp(data: &[u8]) -> f64 {
    if data.len() < 10 || data[0] != MSG_VIDEO_FRAME {
        return 0.0;
    }
    let ts = u64::from_le_bytes(data[1..9].try_into().unwrap_or_default());
    ts as f64
}

/// For a VideoFrame message, return whether it is a key-frame.
#[wasm_bindgen]
pub fn video_frame_is_keyframe(data: &[u8]) -> bool {
    if data.len() < 10 || data[0] != MSG_VIDEO_FRAME {
        return false;
    }
    data[9] != 0
}

/// For a VideoFrame message, return the offset where H.264 data begins.
/// The caller can use this to create a sub-view of the ArrayBuffer.
#[wasm_bindgen]
pub fn video_frame_data_offset() -> usize {
    10
}

/// For a ServerInfo message, extract width.
#[wasm_bindgen]
pub fn server_info_width(data: &[u8]) -> u16 {
    if data.len() < 6 || data[0] != MSG_SERVER_INFO {
        return 0;
    }
    u16::from_le_bytes(data[1..3].try_into().unwrap_or_default())
}

/// For a ServerInfo message, extract height.
#[wasm_bindgen]
pub fn server_info_height(data: &[u8]) -> u16 {
    if data.len() < 6 || data[0] != MSG_SERVER_INFO {
        return 0;
    }
    u16::from_le_bytes(data[3..5].try_into().unwrap_or_default())
}

/// For a ServerInfo message, extract FPS.
#[wasm_bindgen]
pub fn server_info_fps(data: &[u8]) -> u8 {
    if data.len() < 6 || data[0] != MSG_SERVER_INFO {
        return 0;
    }
    data[5]
}

// ---------------------------------------------------------------------------
// Latency tracker – maintains a running average of frame latency.
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub struct LatencyTracker {
    samples: Vec<f64>,
    index: usize,
    capacity: usize,
}

#[wasm_bindgen]
impl LatencyTracker {
    #[wasm_bindgen(constructor)]
    pub fn new(window_size: usize) -> Self {
        let cap = window_size.max(1);
        Self {
            samples: Vec::with_capacity(cap),
            index: 0,
            capacity: cap,
        }
    }

    /// Record a one-way latency sample (in milliseconds).
    pub fn record(&mut self, latency_ms: f64) {
        if self.samples.len() < self.capacity {
            self.samples.push(latency_ms);
        } else {
            self.samples[self.index] = latency_ms;
        }
        self.index = (self.index + 1) % self.capacity;
    }

    /// Average latency over the window.
    pub fn average_ms(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    /// Minimum latency in the window.
    pub fn min_ms(&self) -> f64 {
        self.samples.iter().cloned().fold(f64::MAX, f64::min)
    }

    /// Maximum latency in the window.
    pub fn max_ms(&self) -> f64 {
        self.samples.iter().cloned().fold(f64::MIN, f64::max)
    }

    /// Number of samples collected so far.
    pub fn count(&self) -> usize {
        self.samples.len()
    }
}
