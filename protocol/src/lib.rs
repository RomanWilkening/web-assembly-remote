/// Binary protocol for low-latency remote desktop communication.
///
/// All multi-byte integers are little-endian.
/// Messages are sent as binary WebSocket frames with no additional framing.

// --- Message type constants ---

// Server → Client
pub const MSG_VIDEO_FRAME: u8 = 0x01;
pub const MSG_SERVER_INFO: u8 = 0x02;
pub const MSG_CURSOR_INFO: u8 = 0x03;

// Client → Server
pub const MSG_MOUSE_MOVE: u8 = 0x10;
pub const MSG_MOUSE_BUTTON: u8 = 0x11;
pub const MSG_MOUSE_SCROLL: u8 = 0x12;
pub const MSG_KEY_EVENT: u8 = 0x13;
pub const MSG_CLIENT_READY: u8 = 0x14;

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
    ServerInfo {
        width: u16,
        height: u16,
        fps: u8,
    },
    /// Cursor position update (server-side cursor).
    CursorInfo {
        x: u16,
        y: u16,
        visible: bool,
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
    ClientReady,
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
            ServerMessage::ServerInfo { width, height, fps } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(MSG_SERVER_INFO);
                buf.extend_from_slice(&width.to_le_bytes());
                buf.extend_from_slice(&height.to_le_bytes());
                buf.push(*fps);
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
            ClientMessage::ClientReady => {
                vec![MSG_CLIENT_READY]
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
                Some(ServerMessage::ServerInfo { width, height, fps })
            }
            MSG_CURSOR_INFO if data.len() >= 6 => {
                let x = u16::from_le_bytes(data[1..3].try_into().ok()?);
                let y = u16::from_le_bytes(data[3..5].try_into().ok()?);
                let visible = data[5] != 0;
                Some(ServerMessage::CursorInfo { x, y, visible })
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
            MSG_CLIENT_READY => Some(ClientMessage::ClientReady),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_server_info() {
        let msg = ServerMessage::ServerInfo { width: 1920, height: 1080, fps: 60 };
        let encoded = msg.encode();
        let decoded = ServerMessage::decode(&encoded).unwrap();
        match decoded {
            ServerMessage::ServerInfo { width, height, fps } => {
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
                assert_eq!(fps, 60);
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
    fn decode_empty_returns_none() {
        assert!(ServerMessage::decode(&[]).is_none());
        assert!(ClientMessage::decode(&[]).is_none());
    }

    #[test]
    fn decode_truncated_returns_none() {
        assert!(ServerMessage::decode(&[MSG_SERVER_INFO, 0x00]).is_none());
        assert!(ClientMessage::decode(&[MSG_MOUSE_BUTTON, 0x00]).is_none());
    }
}
