/// Binary protocol for low-latency remote desktop communication.
///
/// All multi-byte integers are little-endian.
/// Messages are sent as binary WebSocket frames with no additional framing.

// --- Message type constants ---

// Server → Client
pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_SERVER_INFO: u8 = 0x02;
pub const MSG_CURSOR_INFO: u8 = 0x03;
pub const MSG_MONITOR_LIST: u8 = 0x04;
pub const MSG_AUDIO_DATA: u8 = 0x05;
pub const MSG_AUDIO_DEVICE_LIST: u8 = 0x06;
pub const MSG_PONG: u8 = 0x07;

// Client → Server
pub const MSG_MOUSE_MOVE: u8 = 0x10;
pub const MSG_MOUSE_BUTTON: u8 = 0x11;
pub const MSG_MOUSE_SCROLL: u8 = 0x12;
pub const MSG_KEY_EVENT: u8 = 0x13;
pub const MSG_CLIENT_READY: u8 = 0x14;
pub const MSG_SELECT_MONITOR: u8 = 0x15;
pub const MSG_SELECT_AUDIO: u8 = 0x16;
pub const MSG_KEY_SCANCODE: u8 = 0x17;
pub const MSG_SET_KEYBOARD_LAYOUT: u8 = 0x18;
pub const MSG_PING: u8 = 0x19;

// --- Monitor info ---

/// Information about a single display/monitor.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Zero-based monitor index.
    pub index: u8,
    /// Horizontal offset in the virtual desktop.
    pub x: i16,
    /// Vertical offset in the virtual desktop.
    pub y: i16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// True if this is the primary monitor.
    pub primary: bool,
}

// --- Audio device info ---

/// Information about an available audio capture device.
#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    /// Zero-based device index.
    pub index: u8,
    /// Human-readable device name (e.g. "Stereo Mix (Realtek …)").
    pub name: String,
}

// --- Server messages ---

#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// Encoded H.264 access unit (one frame).
    VideoFrame {
        /// Microsecond timestamp (server clock) for latency measurement.
        timestamp_us: u64,
        /// True if this is an IDR (key) frame.
        is_keyframe: bool,
        /// Raw H.264 Annex-B data for this access unit.
        data: Vec<u8>,
    },
    /// Initial handshake: desktop resolution and target FPS.
    ///
    /// `codec` selects how the client configures its `VideoDecoder`:
    /// `0` = H.264, `1` = HEVC, `2` = AV1.  Older clients that do not
    /// know about the byte will treat the message as the historical
    /// 6-byte payload and silently fall back to H.264, which matches the
    /// historical default.
    ServerInfo {
        width: u16,
        height: u16,
        fps: u8,
        codec: u8,
    },
    /// Cursor position update (server-side cursor).
    CursorInfo {
        x: u16,
        y: u16,
        visible: bool,
    },
    /// List of available monitors.
    MonitorList {
        monitors: Vec<MonitorInfo>,
    },
    /// Raw audio data (f32le interleaved stereo at 48 kHz).
    AudioData {
        data: Vec<u8>,
    },
    /// List of available audio capture devices.
    AudioDeviceList {
        devices: Vec<AudioDeviceInfo>,
    },
    /// Reply to a client `Ping`. Echoes the client's timestamp verbatim
    /// so the client can compute round-trip time using only its own
    /// monotonic clock (no NTP / clock-sync between server and browser
    /// required).
    Pong {
        /// The exact value the client sent in `ClientMessage::Ping`.
        client_ts_us: u64,
    },
}

// --- Client messages ---

#[derive(Debug, Clone)]
pub enum ClientMessage {
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, pressed: bool, x: u16, y: u16 },
    MouseScroll { delta_x: i16, delta_y: i16 },
    /// `key_code` is a Windows Virtual-Key code (VK_*).
    KeyEvent { key_code: u16, pressed: bool },
    /// Inject a hardware key event by **PS/2 Set 1 scancode** (the
    /// "Parsec method"). The remote interprets the scancode through
    /// its currently active keyboard layout, so the physical key the
    /// user pressed produces the same character it would on a locally
    /// attached keyboard with that layout. `extended` corresponds to
    /// the `0xE0` prefix and is passed to `SendInput` as
    /// `KEYEVENTF_EXTENDEDKEY`.
    KeyScancode { scancode: u16, extended: bool, pressed: bool },
    ClientReady,
    /// Select a monitor by index.
    SelectMonitor { index: u8 },
    /// Select an audio capture device by index, or 0xFF to disable audio.
    SelectAudio { index: u8 },
    /// Switch the keyboard layout used to interpret incoming scancodes
    /// on the remote. `klid` is a Windows Keyboard-Layout-ID such as
    /// `0x0000_0407` (de-DE) or `0x0000_0409` (en-US).
    SetKeyboardLayout { klid: u32 },
    /// Round-trip-time measurement request. The server replies with
    /// `ServerMessage::Pong` echoing `client_ts_us` verbatim so the
    /// client can compute RTT against its own clock.
    Ping { client_ts_us: u64 },
}

// --- Encoding ---

impl ServerMessage {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerMessage::VideoFrame { timestamp_us, is_keyframe, data } => {
                let mut buf = Vec::with_capacity(1 + 8 + 1 + data.len());
                buf.push(MSG_VIDEO_FRAME);
                buf.extend_from_slice(&timestamp_us.to_le_bytes());
                buf.push(u8::from(*is_keyframe));
                buf.extend_from_slice(data);
                buf
            }
            ServerMessage::ServerInfo { width, height, fps, codec } => {
                let mut buf = Vec::with_capacity(7);
                buf.push(MSG_SERVER_INFO);
                buf.extend_from_slice(&width.to_le_bytes());
                buf.extend_from_slice(&height.to_le_bytes());
                buf.push(*fps);
                buf.push(*codec);
                buf
            }
            ServerMessage::CursorInfo { x, y, visible } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(MSG_CURSOR_INFO);
                buf.extend_from_slice(&x.to_le_bytes());
                buf.extend_from_slice(&y.to_le_bytes());
                buf.push(u8::from(*visible));
                buf
            }
            ServerMessage::MonitorList { monitors } => {
                // [0x04] [count: u8] [for each: index u8, x i16, y i16, w u16, h u16, primary u8]
                let mut buf = Vec::with_capacity(2 + monitors.len() * 10);
                buf.push(MSG_MONITOR_LIST);
                buf.push(monitors.len() as u8);
                for m in monitors {
                    buf.push(m.index);
                    buf.extend_from_slice(&m.x.to_le_bytes());
                    buf.extend_from_slice(&m.y.to_le_bytes());
                    buf.extend_from_slice(&m.width.to_le_bytes());
                    buf.extend_from_slice(&m.height.to_le_bytes());
                    buf.push(u8::from(m.primary));
                }
                buf
            }
            ServerMessage::AudioData { data } => {
                let mut buf = Vec::with_capacity(1 + data.len());
                buf.push(MSG_AUDIO_DATA);
                buf.extend_from_slice(data);
                buf
            }
            ServerMessage::AudioDeviceList { devices } => {
                // [0x06] [count: u8] [for each: index u8, name_len u16, name bytes...]
                let mut buf = Vec::with_capacity(2 + devices.len() * 32);
                buf.push(MSG_AUDIO_DEVICE_LIST);
                buf.push(devices.len() as u8);
                for d in devices {
                    buf.push(d.index);
                    let name_bytes = d.name.as_bytes();
                    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
                    buf.extend_from_slice(name_bytes);
                }
                buf
            }
            ServerMessage::Pong { client_ts_us } => {
                let mut buf = Vec::with_capacity(1 + 8);
                buf.push(MSG_PONG);
                buf.extend_from_slice(&client_ts_us.to_le_bytes());
                buf
            }
        }
    }
}

impl ClientMessage {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientMessage::MouseMove { x, y } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(MSG_MOUSE_MOVE);
                buf.extend_from_slice(&x.to_le_bytes());
                buf.extend_from_slice(&y.to_le_bytes());
                buf
            }
            ClientMessage::MouseButton { button, pressed, x, y } => {
                let mut buf = Vec::with_capacity(7);
                buf.push(MSG_MOUSE_BUTTON);
                buf.push(*button);
                buf.push(u8::from(*pressed));
                buf.extend_from_slice(&x.to_le_bytes());
                buf.extend_from_slice(&y.to_le_bytes());
                buf
            }
            ClientMessage::MouseScroll { delta_x, delta_y } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(MSG_MOUSE_SCROLL);
                buf.extend_from_slice(&delta_x.to_le_bytes());
                buf.extend_from_slice(&delta_y.to_le_bytes());
                buf
            }
            ClientMessage::KeyEvent { key_code, pressed } => {
                let mut buf = Vec::with_capacity(4);
                buf.push(MSG_KEY_EVENT);
                buf.extend_from_slice(&key_code.to_le_bytes());
                buf.push(u8::from(*pressed));
                buf
            }
            ClientMessage::KeyScancode { scancode, extended, pressed } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(MSG_KEY_SCANCODE);
                buf.extend_from_slice(&scancode.to_le_bytes());
                buf.push(u8::from(*extended));
                buf.push(u8::from(*pressed));
                buf
            }
            ClientMessage::ClientReady => {
                vec![MSG_CLIENT_READY]
            }
            ClientMessage::SelectMonitor { index } => {
                vec![MSG_SELECT_MONITOR, *index]
            }
            ClientMessage::SelectAudio { index } => {
                vec![MSG_SELECT_AUDIO, *index]
            }
            ClientMessage::SetKeyboardLayout { klid } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(MSG_SET_KEYBOARD_LAYOUT);
                buf.extend_from_slice(&klid.to_le_bytes());
                buf
            }
            ClientMessage::Ping { client_ts_us } => {
                let mut buf = Vec::with_capacity(1 + 8);
                buf.push(MSG_PING);
                buf.extend_from_slice(&client_ts_us.to_le_bytes());
                buf
            }
        }
    }
}

// --- Decoding ---

impl ServerMessage {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        match data[0] {
            MSG_VIDEO_FRAME if data.len() >= 10 => {
                let timestamp_us = u64::from_le_bytes(data[1..9].try_into().ok()?);
                let is_keyframe = data[9] != 0;
                let frame_data = data[10..].to_vec();
                Some(ServerMessage::VideoFrame {
                    timestamp_us,
                    is_keyframe,
                    data: frame_data,
                })
            }
            MSG_SERVER_INFO if data.len() >= 6 => {
                let width = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let height = u16::from_le_bytes(data[3..5].try_into().ok()?);
                let fps = data[5];
                // Codec byte was added in v2 of the protocol; for
                // backwards compatibility default to H.264 (= 0) when
                // the field is absent.
                let codec = data.get(6).copied().unwrap_or(0);
                Some(ServerMessage::ServerInfo { width, height, fps, codec })
            }
            MSG_CURSOR_INFO if data.len() >= 6 => {
                let x = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let y = u16::from_le_bytes(data[3..5].try_into().ok()?);
                let visible = data[5] != 0;
                Some(ServerMessage::CursorInfo { x, y, visible })
            }
            MSG_MONITOR_LIST if data.len() >= 2 => {
                let count = data[1] as usize;
                let expected_len = 2 + count * 10;
                if data.len() < expected_len {
                    return None;
                }
                let mut monitors = Vec::with_capacity(count);
                for i in 0..count {
                    let off = 2 + i * 10;
                    let index = data[off];
                    let x = i16::from_le_bytes(data[off + 1..off + 3].try_into().ok()?);
                    let y = i16::from_le_bytes(data[off + 3..off + 5].try_into().ok()?);
                    let width = u16::from_le_bytes(data[off + 5..off + 7].try_into().ok()?);
                    let height = u16::from_le_bytes(data[off + 7..off + 9].try_into().ok()?);
                    let primary = data[off + 9] != 0;
                    monitors.push(MonitorInfo { index, x, y, width, height, primary });
                }
                Some(ServerMessage::MonitorList { monitors })
            }
            MSG_AUDIO_DATA if data.len() > 1 => {
                Some(ServerMessage::AudioData { data: data[1..].to_vec() })
            }
            MSG_AUDIO_DEVICE_LIST if data.len() >= 2 => {
                let count = data[1] as usize;
                let mut devices = Vec::with_capacity(count);
                let mut pos = 2;
                for _ in 0..count {
                    if pos + 3 > data.len() {
                        return None;
                    }
                    let index = data[pos];
                    let name_len = u16::from_le_bytes(
                        data[pos + 1..pos + 3].try_into().ok()?,
                    ) as usize;
                    pos += 3;
                    if pos + name_len > data.len() {
                        return None;
                    }
                    let name = String::from_utf8_lossy(&data[pos..pos + name_len]).into_owned();
                    pos += name_len;
                    devices.push(AudioDeviceInfo { index, name });
                }
                Some(ServerMessage::AudioDeviceList { devices })
            }
            MSG_PONG if data.len() >= 9 => {
                let client_ts_us = u64::from_le_bytes(data[1..9].try_into().ok()?);
                Some(ServerMessage::Pong { client_ts_us })
            }
            _ => None,
        }
    }
}

impl ClientMessage {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        match data[0] {
            MSG_MOUSE_MOVE if data.len() >= 5 => {
                let x = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let y = u16::from_le_bytes(data[3..5].try_into().ok()?);
                Some(ClientMessage::MouseMove { x, y })
            }
            MSG_MOUSE_BUTTON if data.len() >= 7 => {
                let button = data[1];
                let pressed = data[2] != 0;
                let x = u16::from_le_bytes(data[3..5].try_into().ok()?);
                let y = u16::from_le_bytes(data[5..7].try_into().ok()?);
                Some(ClientMessage::MouseButton { button, pressed, x, y })
            }
            MSG_MOUSE_SCROLL if data.len() >= 5 => {
                let delta_x = i16::from_le_bytes(data[1..3].try_into().ok()?);
                let delta_y = i16::from_le_bytes(data[3..5].try_into().ok()?);
                Some(ClientMessage::MouseScroll { delta_x, delta_y })
            }
            MSG_KEY_EVENT if data.len() >= 4 => {
                let key_code = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let pressed = data[3] != 0;
                Some(ClientMessage::KeyEvent { key_code, pressed })
            }
            MSG_KEY_SCANCODE if data.len() >= 5 => {
                let scancode = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let extended = data[3] != 0;
                let pressed = data[4] != 0;
                Some(ClientMessage::KeyScancode { scancode, extended, pressed })
            }
            MSG_CLIENT_READY => Some(ClientMessage::ClientReady),
            MSG_SELECT_MONITOR if data.len() >= 2 => {
                Some(ClientMessage::SelectMonitor { index: data[1] })
            }
            MSG_SELECT_AUDIO if data.len() >= 2 => {
                Some(ClientMessage::SelectAudio { index: data[1] })
            }
            MSG_SET_KEYBOARD_LAYOUT if data.len() >= 5 => {
                let klid = u32::from_le_bytes(data[1..5].try_into().ok()?);
                Some(ClientMessage::SetKeyboardLayout { klid })
            }
            MSG_PING if data.len() >= 9 => {
                let client_ts_us = u64::from_le_bytes(data[1..9].try_into().ok()?);
                Some(ClientMessage::Ping { client_ts_us })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_server_info() {
        let msg = ServerMessage::ServerInfo { width: 1920, height: 1080, fps: 60, codec: 1 };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::ServerInfo { width, height, fps, codec } => {
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
                assert_eq!(fps, 60);
                assert_eq!(codec, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Old clients (and old proxies that may strip trailing bytes)
    /// must still be able to decode a 6-byte ServerInfo by treating the
    /// missing codec byte as H.264.
    #[test]
    fn roundtrip_server_info_legacy_no_codec_byte() {
        let mut bytes = vec![MSG_SERVER_INFO];
        bytes.extend_from_slice(&1920u16.to_le_bytes());
        bytes.extend_from_slice(&1080u16.to_le_bytes());
        bytes.push(60);
        // Note: no codec byte — legacy 6-byte payload.
        let decoded = ServerMessage::decode(&bytes).unwrap();
        match decoded {
            ServerMessage::ServerInfo { width, height, fps, codec } => {
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
                assert_eq!(fps, 60);
                assert_eq!(codec, 0, "legacy payload must default to H.264");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_video_frame() {
        let msg = ServerMessage::VideoFrame {
            timestamp_us: 123456789,
            is_keyframe: true,
            data: vec![0, 0, 0, 1, 0x65, 0xAA],
        };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::VideoFrame { timestamp_us, is_keyframe, data } => {
                assert_eq!(timestamp_us, 123456789);
                assert!(is_keyframe);
                assert_eq!(data, vec![0, 0, 0, 1, 0x65, 0xAA]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_mouse_move() {
        let msg = ClientMessage::MouseMove { x: 500, y: 300 };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::MouseMove { x, y } => {
                assert_eq!(x, 500);
                assert_eq!(y, 300);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_key_event() {
        let msg = ClientMessage::KeyEvent { key_code: 0x41, pressed: true };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::KeyEvent { key_code, pressed } => {
                assert_eq!(key_code, 0x41);
                assert!(pressed);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_key_scancode() {
        // Non-extended: physical 'Z' position on a US keyboard (PS/2 Set 1 = 0x2C)
        let msg = ClientMessage::KeyScancode { scancode: 0x2C, extended: false, pressed: true };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::KeyScancode { scancode, extended, pressed } => {
                assert_eq!(scancode, 0x2C);
                assert!(!extended);
                assert!(pressed);
            }
            _ => panic!("wrong variant"),
        }

        // Extended: ArrowUp (E0 48)
        let msg = ClientMessage::KeyScancode { scancode: 0x48, extended: true, pressed: false };
        let encoded = msg.encode();
        match ClientMessage::decode(&encoded).unwrap() {
            ClientMessage::KeyScancode { scancode, extended, pressed } => {
                assert_eq!(scancode, 0x48);
                assert!(extended);
                assert!(!pressed);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_set_keyboard_layout() {
        // German (de-DE)
        let msg = ClientMessage::SetKeyboardLayout { klid: 0x0000_0407 };
        let encoded = msg.encode();
        match ClientMessage::decode(&encoded).unwrap() {
            ClientMessage::SetKeyboardLayout { klid } => assert_eq!(klid, 0x0000_0407),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_ping_pong() {
        let ts = 1_700_000_000_000_000_u64;
        let encoded = ClientMessage::Ping { client_ts_us: ts }.encode();
        match ClientMessage::decode(&encoded).unwrap() {
            ClientMessage::Ping { client_ts_us } => assert_eq!(client_ts_us, ts),
            _ => panic!("wrong variant"),
        }

        let encoded = ServerMessage::Pong { client_ts_us: ts }.encode();
        match ServerMessage::decode(&encoded).unwrap() {
            ServerMessage::Pong { client_ts_us } => assert_eq!(client_ts_us, ts),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_empty_returns_none() {
        assert!(ServerMessage::decode(&[]).is_none());
        assert!(ClientMessage::decode(&[]).is_none());
    }

    #[test]
    fn decode_truncated_returns_none() {
        assert!(ServerMessage::decode(&[MSG_SERVER_INFO, 0x00]).is_none());
        assert!(ClientMessage::decode(&[MSG_MOUSE_BUTTON, 0x00]).is_none());
    }

    #[test]
    fn roundtrip_cursor_info() {
        let msg = ServerMessage::CursorInfo { x: 100, y: 200, visible: true };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::CursorInfo { x, y, visible } => {
                assert_eq!(x, 100);
                assert_eq!(y, 200);
                assert!(visible);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_monitor_list() {
        let msg = ServerMessage::MonitorList {
            monitors: vec![
                MonitorInfo { index: 0, x: 0, y: 0, width: 1920, height: 1080, primary: true },
                MonitorInfo { index: 1, x: 1920, y: 0, width: 2560, height: 1440, primary: false },
            ],
        };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::MonitorList { monitors } => {
                assert_eq!(monitors.len(), 2);
                assert_eq!(monitors[0].index, 0);
                assert_eq!(monitors[0].width, 1920);
                assert!(monitors[0].primary);
                assert_eq!(monitors[1].index, 1);
                assert_eq!(monitors[1].x, 1920);
                assert_eq!(monitors[1].width, 2560);
                assert!(!monitors[1].primary);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_select_monitor() {
        let msg = ClientMessage::SelectMonitor { index: 2 };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::SelectMonitor { index } => assert_eq!(index, 2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_audio_data() {
        let pcm = vec![0u8; 7680]; // 20ms of 48kHz stereo f32le
        let msg = ServerMessage::AudioData { data: pcm.clone() };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::AudioData { data } => {
                assert_eq!(data.len(), 7680);
                assert_eq!(data, pcm);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_audio_device_list() {
        let msg = ServerMessage::AudioDeviceList {
            devices: vec![
                AudioDeviceInfo { index: 0, name: "Stereo Mix (Realtek)".into() },
                AudioDeviceInfo { index: 1, name: "Microphone (Realtek)".into() },
            ],
        };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::AudioDeviceList { devices } => {
                assert_eq!(devices.len(), 2);
                assert_eq!(devices[0].index, 0);
                assert_eq!(devices[0].name, "Stereo Mix (Realtek)");
                assert_eq!(devices[1].index, 1);
                assert_eq!(devices[1].name, "Microphone (Realtek)");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_audio_device_list_empty() {
        let msg = ServerMessage::AudioDeviceList { devices: vec![] };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::AudioDeviceList { devices } => {
                assert_eq!(devices.len(), 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_select_audio() {
        let msg = ClientMessage::SelectAudio { index: 3 };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::SelectAudio { index } => assert_eq!(index, 3),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_select_audio_disable() {
        let msg = ClientMessage::SelectAudio { index: 0xFF };
        let encoded = msg.encode();
        let decoded = ClientMessage::decode(&encoded).unwrap();
        match decoded {
            ClientMessage::SelectAudio { index } => assert_eq!(index, 0xFF),
            _ => panic!("wrong variant"),
        }
    }
}
