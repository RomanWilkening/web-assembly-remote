use wasm_bindgen::prelude::*;

// Re-export protocol constants so JS can reference them if needed.
pub use protocol::{
    MSG_AUDIO_DATA, MSG_AUDIO_DEVICE_LIST, MSG_CLIENT_READY, MSG_CURSOR_INFO,
    MSG_ENCODER_LIST, MSG_KEY_EVENT, MSG_KEY_SCANCODE, MSG_MONITOR_LIST, MSG_MOUSE_BUTTON,
    MSG_MOUSE_MOVE, MSG_MOUSE_SCROLL, MSG_PING, MSG_PONG, MSG_PROFILE_LIST,
    MSG_SELECT_AUDIO, MSG_SELECT_ENCODER, MSG_SELECT_MONITOR, MSG_SELECT_PROFILE,
    MSG_SERVER_INFO, MSG_SET_KEYBOARD_LAYOUT, MSG_VIDEO_FRAME,
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

/// Encode a `SelectEncoder` request — switch the live encoder to the
/// one at `index` in the most recent `EncoderList` (Block A).
#[wasm_bindgen]
pub fn encode_select_encoder(index: u8) -> Vec<u8> {
    protocol::ClientMessage::SelectEncoder { index }.encode()
}

/// Encode a `SelectProfile` request — switch to the named profile at
/// `index` in the most recent `ProfileList` (Block C).
#[wasm_bindgen]
pub fn encode_select_profile(index: u8) -> Vec<u8> {
    protocol::ClientMessage::SelectProfile { index }.encode()
}

/// Encode a `Ping` client message containing a client-supplied
/// timestamp (microseconds, opaque to the server). The server echoes
/// it back as `Pong`, allowing the client to compute round-trip time
/// using only its own clock.
///
/// `client_ts_us` is `f64` because JavaScript numbers are doubles;
/// any value in the realistic Unix-microsecond range fits losslessly
/// in 53 bits of mantissa.
#[wasm_bindgen]
pub fn encode_ping(client_ts_us: f64) -> Vec<u8> {
    let ts = if client_ts_us.is_finite() && client_ts_us >= 0.0 {
        client_ts_us as u64
    } else {
        0
    };
    protocol::ClientMessage::Ping { client_ts_us: ts }.encode()
}

// ---------------------------------------------------------------------------
// Decode helpers – called from JavaScript to parse incoming server messages.
//
// NOTE on the hot-path: small fixed-layout headers (VideoFrame, Pong,
// CursorInfo, ServerInfo) are now parsed directly in JS via `DataView`
// instead of routed through wasm-bindgen.  Each `&[u8]` parameter forces
// wasm-bindgen to copy the *entire* `Uint8Array` into the WASM linear
// memory — for a 4K key-frame that is hundreds of KB of needless memcpy
// per frame.  The helpers below are kept only for messages that contain
// variable-length nested data (monitor list, audio device list) where
// implementing the parser in JS is more error-prone than worth.
// ---------------------------------------------------------------------------

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
// EncoderList decode helpers (Block A)
//
// Wire layout, byte-by-byte:
//   [0x08] [count: u8] [for each encoder:
//      index u8, codec u8, hw_vendor u8, active u8,
//      name_len u16 LE, name bytes...]
//
// Mirrors the JS `audio-select` pattern so the toolbar dropdown can be
// populated with one ~30 LOC handler.
// ---------------------------------------------------------------------------

/// Number of encoders advertised in an `EncoderList` message.
/// Returns `0` for malformed input or wrong message type.
#[wasm_bindgen]
pub fn encoder_list_count(data: &[u8]) -> u8 {
    if data.len() < 2 || data[0] != MSG_ENCODER_LIST {
        return 0;
    }
    data[1]
}

/// Locate the start byte of encoder entry `i` inside an `EncoderList`
/// payload, or `None` when `i` is out of range / the buffer is truncated.
fn encoder_entry_offset(data: &[u8], i: u8) -> Option<usize> {
    if data.len() < 2 || data[0] != MSG_ENCODER_LIST {
        return None;
    }
    let count = data[1];
    if i >= count {
        return None;
    }
    let mut pos: usize = 2;
    for n in 0..=i {
        if pos + 6 > data.len() {
            return None;
        }
        let name_len = u16::from_le_bytes(
            data[pos + 4..pos + 6].try_into().unwrap_or_default(),
        ) as usize;
        if pos + 6 + name_len > data.len() {
            return None;
        }
        if n == i {
            return Some(pos);
        }
        pos += 6 + name_len;
    }
    None
}

#[wasm_bindgen]
pub fn encoder_index(data: &[u8], i: u8) -> u8 {
    encoder_entry_offset(data, i).map(|p| data[p]).unwrap_or(0)
}

#[wasm_bindgen]
pub fn encoder_codec(data: &[u8], i: u8) -> u8 {
    encoder_entry_offset(data, i).map(|p| data[p + 1]).unwrap_or(0)
}

#[wasm_bindgen]
pub fn encoder_hw_vendor(data: &[u8], i: u8) -> u8 {
    encoder_entry_offset(data, i).map(|p| data[p + 2]).unwrap_or(0)
}

#[wasm_bindgen]
pub fn encoder_is_active(data: &[u8], i: u8) -> bool {
    encoder_entry_offset(data, i)
        .map(|p| data[p + 3] != 0)
        .unwrap_or(false)
}

#[wasm_bindgen]
pub fn encoder_name(data: &[u8], i: u8) -> String {
    if let Some(p) = encoder_entry_offset(data, i) {
        let name_len = u16::from_le_bytes(
            data[p + 4..p + 6].try_into().unwrap_or_default(),
        ) as usize;
        return String::from_utf8_lossy(&data[p + 6..p + 6 + name_len]).into_owned();
    }
    String::new()
}

// ---------------------------------------------------------------------------
// ProfileList decode helpers (Block C)
//
// Wire layout:
//   [0x09] [count: u8] [for each:
//      index u8, active u8, name_len u16 LE, name bytes...]
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub fn profile_list_count(data: &[u8]) -> u8 {
    if data.len() < 2 || data[0] != MSG_PROFILE_LIST {
        return 0;
    }
    data[1]
}

fn profile_entry_offset(data: &[u8], i: u8) -> Option<usize> {
    if data.len() < 2 || data[0] != MSG_PROFILE_LIST {
        return None;
    }
    let count = data[1];
    if i >= count {
        return None;
    }
    let mut pos: usize = 2;
    for n in 0..=i {
        if pos + 4 > data.len() {
            return None;
        }
        let name_len = u16::from_le_bytes(
            data[pos + 2..pos + 4].try_into().unwrap_or_default(),
        ) as usize;
        if pos + 4 + name_len > data.len() {
            return None;
        }
        if n == i {
            return Some(pos);
        }
        pos += 4 + name_len;
    }
    None
}

#[wasm_bindgen]
pub fn profile_index(data: &[u8], i: u8) -> u8 {
    profile_entry_offset(data, i).map(|p| data[p]).unwrap_or(0)
}

#[wasm_bindgen]
pub fn profile_is_active(data: &[u8], i: u8) -> bool {
    profile_entry_offset(data, i)
        .map(|p| data[p + 1] != 0)
        .unwrap_or(false)
}

#[wasm_bindgen]
pub fn profile_name(data: &[u8], i: u8) -> String {
    if let Some(p) = profile_entry_offset(data, i) {
        let name_len = u16::from_le_bytes(
            data[p + 2..p + 4].try_into().unwrap_or_default(),
        ) as usize;
        return String::from_utf8_lossy(&data[p + 4..p + 4 + name_len]).into_owned();
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{EncoderInfo, ProfileInfo, ServerMessage};

    #[test]
    fn encoder_list_helpers_roundtrip() {
        let msg = ServerMessage::EncoderList {
            encoders: vec![
                EncoderInfo { index: 0, name: "h264_amf".into(), codec: 0, hw_vendor: 0, active: true },
                EncoderInfo { index: 1, name: "libsvtav1".into(), codec: 2, hw_vendor: 4, active: false },
            ],
        };
        let buf = msg.encode();
        assert_eq!(encoder_list_count(&buf), 2);
        assert_eq!(encoder_index(&buf, 0), 0);
        assert_eq!(encoder_codec(&buf, 0), 0);
        assert_eq!(encoder_hw_vendor(&buf, 0), 0);
        assert!(encoder_is_active(&buf, 0));
        assert_eq!(encoder_name(&buf, 0), "h264_amf");
        assert_eq!(encoder_index(&buf, 1), 1);
        assert_eq!(encoder_codec(&buf, 1), 2);
        assert_eq!(encoder_hw_vendor(&buf, 1), 4);
        assert!(!encoder_is_active(&buf, 1));
        assert_eq!(encoder_name(&buf, 1), "libsvtav1");
    }

    #[test]
    fn encoder_list_helpers_handle_out_of_range() {
        let msg = ServerMessage::EncoderList {
            encoders: vec![EncoderInfo {
                index: 0, name: "x".into(), codec: 0, hw_vendor: 4, active: true,
            }],
        };
        let buf = msg.encode();
        assert_eq!(encoder_list_count(&buf), 1);
        // Out-of-range index returns sentinel values (no panic).
        assert_eq!(encoder_index(&buf, 5), 0);
        assert_eq!(encoder_name(&buf, 5), "");
        assert!(!encoder_is_active(&buf, 5));
    }

    #[test]
    fn encoder_list_helpers_reject_wrong_message_type() {
        // Passing an AudioDeviceList must yield 0/empty, not panic.
        let bogus = vec![MSG_AUDIO_DEVICE_LIST, 1, 0, 1, 0, b'X'];
        assert_eq!(encoder_list_count(&bogus), 0);
        assert_eq!(encoder_name(&bogus, 0), "");
    }

    #[test]
    fn profile_list_helpers_roundtrip() {
        let msg = ServerMessage::ProfileList {
            profiles: vec![
                ProfileInfo { index: 0, name: "gaming".into(), active: true },
                ProfileInfo { index: 1, name: "office".into(), active: false },
            ],
        };
        let buf = msg.encode();
        assert_eq!(profile_list_count(&buf), 2);
        assert_eq!(profile_index(&buf, 0), 0);
        assert!(profile_is_active(&buf, 0));
        assert_eq!(profile_name(&buf, 0), "gaming");
        assert_eq!(profile_name(&buf, 1), "office");
        assert!(!profile_is_active(&buf, 1));
    }

    #[test]
    fn select_encoder_and_profile_encode_round_trip() {
        let buf = encode_select_encoder(7);
        match protocol::ClientMessage::decode(&buf).unwrap() {
            protocol::ClientMessage::SelectEncoder { index } => assert_eq!(index, 7),
            _ => panic!("wrong variant"),
        }
        let buf = encode_select_profile(2);
        match protocol::ClientMessage::decode(&buf).unwrap() {
            protocol::ClientMessage::SelectProfile { index } => assert_eq!(index, 2),
            _ => panic!("wrong variant"),
        }
    }
}
