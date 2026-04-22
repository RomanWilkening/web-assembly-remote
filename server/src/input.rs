use protocol::ClientMessage;

/// Simulate mouse / keyboard input on the host desktop.
///
/// On Windows this uses the `SendInput` Win32 API.
/// On other platforms it is a no-op (compile-only stub for CI).
///
/// The simulator is aware of the currently captured monitor's position
/// within the virtual desktop so that mouse coordinates sent by the
/// client (relative to the monitor) are mapped to the correct absolute
/// position and clamped to the monitor's bounds.
pub struct InputSimulator {
    /// Top-left X of the captured monitor in virtual-desktop coordinates.
    #[allow(dead_code)]
    monitor_x: i32,
    /// Top-left Y of the captured monitor in virtual-desktop coordinates.
    #[allow(dead_code)]
    monitor_y: i32,
    /// Width of the captured monitor in pixels.
    #[allow(dead_code)]
    monitor_w: u32,
    /// Height of the captured monitor in pixels.
    #[allow(dead_code)]
    monitor_h: u32,
}

impl InputSimulator {
    /// Create a new simulator bound to the given monitor geometry.
    pub fn new(monitor_x: i32, monitor_y: i32, monitor_w: u32, monitor_h: u32) -> Self {
        Self {
            monitor_x,
            monitor_y,
            monitor_w,
            monitor_h,
        }
    }

    pub fn handle(&self, msg: ClientMessage) {
        match msg {
            ClientMessage::MouseMove { x, y } => self.mouse_move(x, y),
            ClientMessage::MouseButton { button, pressed, x, y } => {
                self.mouse_move(x, y);
                Self::mouse_button(button, pressed);
            }
            ClientMessage::MouseScroll { delta_x: _, delta_y } => {
                Self::mouse_scroll(delta_y);
            }
            ClientMessage::KeyEvent { key_code, pressed } => {
                Self::key_event(key_code, pressed);
            }
            ClientMessage::KeyScancode { scancode, extended, pressed } => {
                Self::key_scancode(scancode, extended, pressed);
            }
            ClientMessage::ClientReady => { /* nothing to do */ }
            ClientMessage::SelectMonitor { .. } => { /* handled by server */ }
            ClientMessage::SelectAudio { .. } => { /* handled by server */ }
            ClientMessage::SetKeyboardLayout { klid } => {
                Self::set_keyboard_layout(klid);
            }
            ClientMessage::Ping { .. } => { /* handled by server */ }
        }
    }

    // ── Windows implementation ──────────────────────────────────

    #[cfg(windows)]
    fn mouse_move(&self, x: u16, y: u16) {
        use winapi::um::winuser::{
            GetSystemMetrics, SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, SM_CXVIRTUALSCREEN,
            SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        };

        // Clamp client coordinates to the monitor dimensions.
        let cx = (x as u32).min(self.monitor_w.saturating_sub(1));
        let cy = (y as u32).min(self.monitor_h.saturating_sub(1));

        // Convert to absolute virtual-desktop position.
        let abs_x = self.monitor_x + cx as i32;
        let abs_y = self.monitor_y + cy as i32;

        // Virtual-desktop origin and size (spans all monitors).
        let virt_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let virt_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let virt_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
        let virt_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);

        // Normalise to 0‥65535 range relative to the virtual desktop.
        let norm_x = ((abs_x - virt_x) as u32 * 65535 / virt_w as u32) as i32;
        let norm_y = ((abs_y - virt_y) as u32 * 65535 / virt_h as u32) as i32;

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

    /// Inject a hardware key event by PS/2 Set 1 scancode. The remote
    /// translates the scancode through its currently active keyboard
    /// layout, so the same physical key produces the same character it
    /// would on a directly attached keyboard with that layout
    /// ("Parsec method"). `extended` toggles `KEYEVENTF_EXTENDEDKEY`
    /// (the 0xE0 prefix) which is required for cursor keys, the right
    /// Ctrl/Alt/Win, the numpad-Enter and -Divide, Insert/Delete/Home/
    /// End/PgUp/PgDn, and similar.
    #[cfg(windows)]
    fn key_scancode(scancode: u16, extended: bool, pressed: bool) {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_KEYBOARD, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            KEYEVENTF_SCANCODE,
        };

        let mut flags: u32 = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }

        let mut input = INPUT::default();
        input.type_ = INPUT_KEYBOARD;
        unsafe {
            let ki = input.u.ki_mut();
            ki.wVk = 0;
            ki.wScan = scancode;
            ki.dwFlags = flags;
            SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// Switch the keyboard layout used by the foreground window to the
    /// requested KLID (e.g. `0x0000_0407` for de-DE). This is the
    /// standard Win32 way of changing the input language used by the
    /// language bar: load the layout, then ask the foreground window
    /// to switch its input language to it.
    #[cfg(windows)]
    fn set_keyboard_layout(klid: u32) {
        use std::iter::once;
        use winapi::shared::minwindef::{LPARAM, WPARAM};
        use winapi::um::winuser::{
            GetForegroundWindow, LoadKeyboardLayoutW, PostMessageW, KLF_ACTIVATE,
            WM_INPUTLANGCHANGEREQUEST,
        };

        // KLIDs are 8 hex digits, e.g. "00000407" for de-DE.
        let klid_str: String = format!("{klid:08X}");
        let wide: Vec<u16> = klid_str.encode_utf16().chain(once(0)).collect();

        unsafe {
            let hkl = LoadKeyboardLayoutW(wide.as_ptr(), KLF_ACTIVATE);
            if hkl.is_null() {
                log::warn!("LoadKeyboardLayoutW failed for KLID {klid_str}");
                return;
            }
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                log::warn!("No foreground window to send WM_INPUTLANGCHANGEREQUEST to");
                return;
            }
            // INPUTLANGCHANGE_SYSCHARSET is intentionally not set – we
            // just want the foreground window to honour the new layout.
            // HKL is a HANDLE (pointer); convert via usize for an
            // explicit, lossless cast to LPARAM (which is isize).
            PostMessageW(hwnd, WM_INPUTLANGCHANGEREQUEST, 0 as WPARAM, hkl as usize as LPARAM);
            log::info!("Requested keyboard layout switch to {klid_str}");
        }
    }

    // ── Non-Windows stubs (allow compilation on Linux CI) ──────

    #[cfg(not(windows))]
    fn mouse_move(&self, _x: u16, _y: u16) {
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

    #[cfg(not(windows))]
    fn key_scancode(_scancode: u16, _extended: bool, _pressed: bool) {
        log::trace!("key_scancode stub");
    }

    #[cfg(not(windows))]
    fn set_keyboard_layout(_klid: u32) {
        log::trace!("set_keyboard_layout stub");
    }
}
