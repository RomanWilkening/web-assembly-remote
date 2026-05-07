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
pub const MSG_ENCODER_LIST: u8 = 0x08;
pub const MSG_PROFILE_LIST: u8 = 0x09;

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
pub const MSG_SELECT_ENCODER: u8 = 0x1A;
pub const MSG_SELECT_PROFILE: u8 = 0x1B;

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

// --- Encoder info (Block A) ---

/// Information about one FFmpeg encoder the server has probed at startup.
///
/// Sent in [`ServerMessage::EncoderList`] so the client toolbar can
/// populate its "Encoder" dropdown with **only** the encoders that work
/// on this particular machine — manual `--encoder h264_amf` guessing
/// is no longer required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Stable index assigned by the server for use in
    /// [`ClientMessage::SelectEncoder`].
    pub index: u8,
    /// FFmpeg encoder name, e.g. `"h264_amf"`, `"libsvtav1"`.
    pub name: String,
    /// Codec family: `0` = H.264, `1` = HEVC, `2` = AV1
    /// (matches `CodecKind::protocol_id`).
    pub codec: u8,
    /// Hardware vendor: `0` = AMD, `1` = NVIDIA, `2` = Intel,
    /// `3` = Microsoft (MF), `4` = Software, `5` = Other.
    pub hw_vendor: u8,
    /// True if the encoder is currently the active one.
    pub active: bool,
}

/// Information about one named encoder profile from `[encoder.profiles.*]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub index: u8,
    /// Profile name, e.g. `"gaming"`, `"office"`.
    pub name: String,
    /// True if this profile is the active one.
    pub active: bool,
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
    /// List of FFmpeg encoders that the server has probed and confirmed
    /// to work on this machine.  Sent once after `ServerInfo` so the
    /// client toolbar can populate its "Encoder" dropdown.  Older
    /// clients ignore the unknown message ID per the wire-protocol
    /// forward-compatibility rules.
    EncoderList {
        encoders: Vec<EncoderInfo>,
    },
    /// List of named encoder profiles configured on the server.  Sent
    /// once after `EncoderList`; same forward-compatibility story.
    ProfileList {
        profiles: Vec<ProfileInfo>,
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
    /// Switch the live encoder to the one at `index` in the most-recent
    /// `EncoderList`.  The server may ignore the request when the
    /// requested encoder is no longer available; nothing visible to the
    /// client breaks if it is.
    SelectEncoder { index: u8 },
    /// Switch the live encoder to the named profile from
    /// `[encoder.profiles.*]` at `index` in the most-recent
    /// `ProfileList`.
    SelectProfile { index: u8 },
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
            ServerMessage::EncoderList { encoders } => {
                // [0x08] [count: u8] [for each:
                //   index u8, codec u8, hw_vendor u8, active u8,
                //   name_len u16, name bytes...]
                let mut buf = Vec::with_capacity(2 + encoders.len() * 32);
                buf.push(MSG_ENCODER_LIST);
                buf.push(encoders.len() as u8);
                for e in encoders {
                    buf.push(e.index);
                    buf.push(e.codec);
                    buf.push(e.hw_vendor);
                    buf.push(u8::from(e.active));
                    let name_bytes = e.name.as_bytes();
                    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
                    buf.extend_from_slice(name_bytes);
                }
                buf
            }
            ServerMessage::ProfileList { profiles } => {
                // [0x09] [count: u8] [for each:
                //   index u8, active u8, name_len u16, name bytes...]
                let mut buf = Vec::with_capacity(2 + profiles.len() * 16);
                buf.push(MSG_PROFILE_LIST);
                buf.push(profiles.len() as u8);
                for p in profiles {
                    buf.push(p.index);
                    buf.push(u8::from(p.active));
                    let name_bytes = p.name.as_bytes();
                    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
                    buf.extend_from_slice(name_bytes);
                }
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
            ClientMessage::SelectEncoder { index } => {
                vec![MSG_SELECT_ENCODER, *index]
            }
            ClientMessage::SelectProfile { index } => {
                vec![MSG_SELECT_PROFILE, *index]
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
            MSG_ENCODER_LIST if data.len() >= 2 => {
                let count = data[1] as usize;
                let mut encoders = Vec::with_capacity(count);
                let mut pos = 2;
                for _ in 0..count {
                    if pos + 6 > data.len() {
                        return None;
                    }
                    let index = data[pos];
                    let codec = data[pos + 1];
                    let hw_vendor = data[pos + 2];
                    let active = data[pos + 3] != 0;
                    let name_len = u16::from_le_bytes(
                        data[pos + 4..pos + 6].try_into().ok()?,
                    ) as usize;
                    pos += 6;
                    if pos + name_len > data.len() {
                        return None;
                    }
                    let name = String::from_utf8_lossy(&data[pos..pos + name_len]).into_owned();
                    pos += name_len;
                    encoders.push(EncoderInfo { index, name, codec, hw_vendor, active });
                }
                Some(ServerMessage::EncoderList { encoders })
            }
            MSG_PROFILE_LIST if data.len() >= 2 => {
                let count = data[1] as usize;
                let mut profiles = Vec::with_capacity(count);
                let mut pos = 2;
                for _ in 0..count {
                    if pos + 4 > data.len() {
                        return None;
                    }
                    let index = data[pos];
                    let active = data[pos + 1] != 0;
                    let name_len = u16::from_le_bytes(
                        data[pos + 2..pos + 4].try_into().ok()?,
                    ) as usize;
                    pos += 4;
                    if pos + name_len > data.len() {
                        return None;
                    }
                    let name = String::from_utf8_lossy(&data[pos..pos + name_len]).into_owned();
                    pos += name_len;
                    profiles.push(ProfileInfo { index, name, active });
                }
                Some(ServerMessage::ProfileList { profiles })
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
            MSG_SELECT_ENCODER if data.len() >= 2 => {
                Some(ClientMessage::SelectEncoder { index: data[1] })
            }
            MSG_SELECT_PROFILE if data.len() >= 2 => {
                Some(ClientMessage::SelectProfile { index: data[1] })
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

    #[test]
    fn roundtrip_encoder_list() {
        let msg = ServerMessage::EncoderList {
            encoders: vec![
                EncoderInfo {
                    index: 0,
                    name: "h264_amf".into(),
                    codec: 0,
                    hw_vendor: 0,
                    active: true,
                },
                EncoderInfo {
                    index: 1,
                    name: "libsvtav1".into(),
                    codec: 2,
                    hw_vendor: 4,
                    active: false,
                },
            ],
        };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::EncoderList { encoders } => {
                assert_eq!(encoders.len(), 2);
                assert_eq!(encoders[0].name, "h264_amf");
                assert!(encoders[0].active);
                assert_eq!(encoders[1].codec, 2);
                assert_eq!(encoders[1].hw_vendor, 4);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_encoder_list_empty() {
        let msg = ServerMessage::EncoderList { encoders: vec![] };
        let decoded = ServerMessage::decode(&msg.encode()).unwrap();
        match decoded {
            ServerMessage::EncoderList { encoders } => assert!(encoders.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_profile_list() {
        let msg = ServerMessage::ProfileList {
            profiles: vec![
                ProfileInfo { index: 0, name: "gaming".into(), active: true },
                ProfileInfo { index: 1, name: "office".into(), active: false },
            ],
        };
        let decoded = ServerMessage::decode(&msg.encode()).unwrap();
        match decoded {
            ServerMessage::ProfileList { profiles } => {
                assert_eq!(profiles.len(), 2);
                assert_eq!(profiles[0].name, "gaming");
                assert!(profiles[0].active);
                assert_eq!(profiles[1].name, "office");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_select_encoder() {
        let msg = ClientMessage::SelectEncoder { index: 5 };
        match ClientMessage::decode(&msg.encode()).unwrap() {
            ClientMessage::SelectEncoder { index } => assert_eq!(index, 5),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_select_profile() {
        let msg = ClientMessage::SelectProfile { index: 2 };
        match ClientMessage::decode(&msg.encode()).unwrap() {
            ClientMessage::SelectProfile { index } => assert_eq!(index, 2),
            _ => panic!("wrong variant"),
        }
    }

    /// Older clients must keep working: when the server sends a future
    /// message ID they don't recognise, `decode` returns `None` and the
    /// dispatcher silently skips it.  This test pins that contract.
    #[test]
    fn unknown_message_id_decodes_to_none() {
        // 0xFE is reserved / future use.
        assert!(ServerMessage::decode(&[0xFE, 0, 0, 0]).is_none());
        assert!(ClientMessage::decode(&[0xFE, 0, 0, 0]).is_none());
    }

    /// Truncated EncoderList payloads must not panic — `decode` returns
    /// `None` and the caller skips the bad message.
    #[test]
    fn encoder_list_truncated_decodes_to_none() {
        // Says count=2 but contains only enough bytes for one entry header.
        let bytes = [MSG_ENCODER_LIST, 2, 0, 0, 0, 0, 4, 0];
        assert!(ServerMessage::decode(&bytes).is_none());
    }

    /// Truncated ProfileList payloads must not panic.
    #[test]
    fn profile_list_truncated_decodes_to_none() {
        let bytes = [MSG_PROFILE_LIST, 5, 0, 1];
        assert!(ServerMessage::decode(&bytes).is_none());
    }
}
