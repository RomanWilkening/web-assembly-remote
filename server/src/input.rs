use protocol::ClientMessage;

/// Simulate mouse / keyboard input on the host desktop.
///
/// On Windows this uses the `SendInput` Win32 API.
/// On other platforms it is a no-op (compile-only stub for CI).
pub struct InputSimulator;

impl InputSimulator {
    pub fn handle(msg: ClientMessage) {
        match msg {
            ClientMessage::MouseMove { x, y } => Self::mouse_move(x, y),
            ClientMessage::MouseButton { button, pressed, x, y } => {
                Self::mouse_move(x, y);
                Self::mouse_button(button, pressed);
            }
            ClientMessage::MouseScroll { delta_x: _, delta_y } => {
                Self::mouse_scroll(delta_y);
            }
            ClientMessage::KeyEvent { key_code, pressed } => {
                Self::key_event(key_code, pressed);
            }
            ClientMessage::ClientReady => { /* nothing to do */ }
        }
    }

    // ── Windows implementation ──────────────────────────────────

    #[cfg(windows)]
    fn mouse_move(x: u16, y: u16) {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_VIRTUALDESK,
        };

        let screen_w = unsafe { winapi::um::winuser::GetSystemMetrics(0) } as u32;
        let screen_h = unsafe { winapi::um::winuser::GetSystemMetrics(1) } as u32;

        // Normalize to 0‥65535 range required by absolute mouse input.
        let norm_x = ((x as u32) * 65535 / screen_w.max(1)) as i32;
        let norm_y = ((y as u32) * 65535 / screen_h.max(1)) as i32;

        let mut input = INPUT::default();
        input.type_ = INPUT_MOUSE;
        unsafe {
            let mi = input.u.mi_mut();
            mi.dx = norm_x;
            mi.dy = norm_y;
            mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    #[cfg(windows)]
    fn mouse_button(button: u8, pressed: bool) {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_MOUSE,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        };

        let flags = match (button, pressed) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_MIDDLEDOWN,
            (1, false) => MOUSEEVENTF_MIDDLEUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            _ => return,
        };

        let mut input = INPUT::default();
        input.type_ = INPUT_MOUSE;
        unsafe {
            input.u.mi_mut().dwFlags = flags;
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    #[cfg(windows)]
    fn mouse_scroll(delta_y: i16) {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_WHEEL,
        };

        let mut input = INPUT::default();
        input.type_ = INPUT_MOUSE;
        unsafe {
            let mi = input.u.mi_mut();
            mi.dwFlags = MOUSEEVENTF_WHEEL;
            mi.mouseData = delta_y as u32;
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    #[cfg(windows)]
    fn key_event(vk: u16, pressed: bool) {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP,
        };

        let mut input = INPUT::default();
        input.type_ = INPUT_KEYBOARD;
        unsafe {
            let ki = input.u.ki_mut();
            ki.wVk = vk;
            if !pressed {
                ki.dwFlags = KEYEVENTF_KEYUP;
            }
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    // ── Non-Windows stubs (allow compilation on Linux CI) ──────

    #[cfg(not(windows))]
    fn mouse_move(_x: u16, _y: u16) {
        log::trace!("mouse_move stub");
    }

    #[cfg(not(windows))]
    fn mouse_button(_button: u8, _pressed: bool) {
        log::trace!("mouse_button stub");
    }

    #[cfg(not(windows))]
    fn mouse_scroll(_delta_y: i16) {
        log::trace!("mouse_scroll stub");
    }

    #[cfg(not(windows))]
    fn key_event(_vk: u16, _pressed: bool) {
        log::trace!("key_event stub");
    }
}
