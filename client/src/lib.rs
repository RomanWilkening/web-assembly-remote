use wasm_bindgen::prelude::*;

// Re-export protocol constants so JS can reference them if needed.
pub use protocol::{
    MSG_AUDIO_DATA, MSG_AUDIO_DEVICE_LIST, MSG_CLIENT_READY, MSG_CURSOR_INFO, MSG_KEY_EVENT,
    MSG_KEY_SCANCODE, MSG_MONITOR_LIST, MSG_MOUSE_BUTTON, MSG_MOUSE_MOVE, MSG_MOUSE_SCROLL,
    MSG_SELECT_AUDIO, MSG_SELECT_MONITOR, MSG_SERVER_INFO, MSG_SET_KEYBOARD_LAYOUT,
    MSG_VIDEO_FRAME,
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

/// Encode a hardware-scancode key event (Parsec-style forwarding).
/// `scancode` is a PS/2 Set 1 scancode; `extended` corresponds to the
/// `0xE0` prefix (cursor keys, right-hand modifiers, numpad enter,
/// etc.). The remote interprets the scancode through its currently
/// active keyboard layout.
#[wasm_bindgen]
pub fn encode_key_scancode(scancode: u16, extended: bool, pressed: bool) -> Vec<u8> {
    protocol::ClientMessage::KeyScancode { scancode, extended, pressed }.encode()
}

/// Switch the active keyboard layout on the remote. `klid` is a
/// Windows Keyboard-Layout-ID, e.g. `0x0000_0407` for de-DE.
#[wasm_bindgen]
pub fn encode_set_keyboard_layout(klid: u32) -> Vec<u8> {
    protocol::ClientMessage::SetKeyboardLayout { klid }.encode()
}

#[wasm_bindgen]
pub fn encode_select_monitor(index: u8) -> Vec<u8> {
    protocol::ClientMessage::SelectMonitor { index }.encode()
}

#[wasm_bindgen]
pub fn encode_select_audio(index: u8) -> Vec<u8> {
    protocol::ClientMessage::SelectAudio { index }.encode()
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
// Cursor info decode helpers
// ---------------------------------------------------------------------------

/// For a CursorInfo message, extract X position.
#[wasm_bindgen]
pub fn cursor_info_x(data: &[u8]) -> u16 {
    if data.len() < 6 || data[0] != MSG_CURSOR_INFO {
        return 0;
    }
    u16::from_le_bytes(data[1..3].try_into().unwrap_or_default())
}

/// For a CursorInfo message, extract Y position.
#[wasm_bindgen]
pub fn cursor_info_y(data: &[u8]) -> u16 {
    if data.len() < 6 || data[0] != MSG_CURSOR_INFO {
        return 0;
    }
    u16::from_le_bytes(data[3..5].try_into().unwrap_or_default())
}

/// For a CursorInfo message, extract visibility.
#[wasm_bindgen]
pub fn cursor_info_visible(data: &[u8]) -> bool {
    if data.len() < 6 || data[0] != MSG_CURSOR_INFO {
        return false;
    }
    data[5] != 0
}

// ---------------------------------------------------------------------------
// Monitor list decode helpers
// ---------------------------------------------------------------------------

/// For a MonitorList message, extract the number of monitors.
#[wasm_bindgen]
pub fn monitor_list_count(data: &[u8]) -> u8 {
    if data.len() < 2 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    data[1]
}

/// For a MonitorList message, extract a monitor's index.
#[wasm_bindgen]
pub fn monitor_info_index(data: &[u8], i: u8) -> u8 {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    data[off]
}

/// For a MonitorList message, extract a monitor's X offset.
#[wasm_bindgen]
pub fn monitor_info_x(data: &[u8], i: u8) -> i16 {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    i16::from_le_bytes(data[off + 1..off + 3].try_into().unwrap_or_default())
}

/// For a MonitorList message, extract a monitor's Y offset.
#[wasm_bindgen]
pub fn monitor_info_y(data: &[u8], i: u8) -> i16 {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    i16::from_le_bytes(data[off + 3..off + 5].try_into().unwrap_or_default())
}

/// For a MonitorList message, extract a monitor's width.
#[wasm_bindgen]
pub fn monitor_info_width(data: &[u8], i: u8) -> u16 {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    u16::from_le_bytes(data[off + 5..off + 7].try_into().unwrap_or_default())
}

/// For a MonitorList message, extract a monitor's height.
#[wasm_bindgen]
pub fn monitor_info_height(data: &[u8], i: u8) -> u16 {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return 0;
    }
    u16::from_le_bytes(data[off + 7..off + 9].try_into().unwrap_or_default())
}

/// For a MonitorList message, check if a monitor is primary.
#[wasm_bindgen]
pub fn monitor_info_primary(data: &[u8], i: u8) -> bool {
    let off = 2 + (i as usize) * 10;
    if data.len() < off + 10 || data[0] != MSG_MONITOR_LIST {
        return false;
    }
    data[off + 9] != 0
}

// ---------------------------------------------------------------------------
// Audio device list decode helpers
// ---------------------------------------------------------------------------

/// For an AudioDeviceList message, extract the number of devices.
#[wasm_bindgen]
pub fn audio_device_list_count(data: &[u8]) -> u8 {
    if data.len() < 2 || data[0] != MSG_AUDIO_DEVICE_LIST {
        return 0;
    }
    data[1]
}

/// For an AudioDeviceList message, extract a device's index.
#[wasm_bindgen]
pub fn audio_device_index(data: &[u8], i: u8) -> u8 {
    if data.len() < 2 || data[0] != MSG_AUDIO_DEVICE_LIST {
        return 0;
    }
    let mut pos: usize = 2;
    for n in 0..=i {
        if pos + 3 > data.len() {
            return 0;
        }
        let idx = data[pos];
        let name_len = u16::from_le_bytes(
            data[pos + 1..pos + 3].try_into().unwrap_or_default(),
        ) as usize;
        pos += 3;
        if pos + name_len > data.len() {
            return 0;
        }
        if n == i {
            return idx;
        }
        pos += name_len;
    }
    0
}

/// For an AudioDeviceList message, extract a device's name as a string.
#[wasm_bindgen]
pub fn audio_device_name(data: &[u8], i: u8) -> String {
    if data.len() < 2 || data[0] != MSG_AUDIO_DEVICE_LIST {
        return String::new();
    }
    let mut pos: usize = 2;
    for n in 0..=i {
        if pos + 3 > data.len() {
            return String::new();
        }
        let name_len = u16::from_le_bytes(
            data[pos + 1..pos + 3].try_into().unwrap_or_default(),
        ) as usize;
        pos += 3;
        if pos + name_len > data.len() {
            return String::new();
        }
        if n == i {
            return String::from_utf8_lossy(&data[pos..pos + name_len]).into_owned();
        }
        pos += name_len;
    }
    String::new()
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
