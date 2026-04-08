/// Captures the current cursor position on the host desktop.
///
/// On Windows, uses the Win32 `GetCursorPos` API.
/// On other platforms, returns a default position (compile-only stub for CI).

/// Get the current cursor position relative to the virtual desktop.
/// Returns (x, y, visible).
#[cfg(windows)]
pub fn get_cursor_position() -> (u16, u16, bool) {
    use winapi::shared::windef::POINT;
    use winapi::um::winuser::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };

    let ok = unsafe { GetCursorInfo(&mut ci) };
    if ok == 0 {
        return (0, 0, false);
    }

    let visible = (ci.flags & CURSOR_SHOWING) != 0;
    let x = ci.ptScreenPos.x.max(0) as u16;
    let y = ci.ptScreenPos.y.max(0) as u16;

    (x, y, visible)
}

#[cfg(not(windows))]
pub fn get_cursor_position() -> (u16, u16, bool) {
    log::trace!("get_cursor_position stub");
    (0, 0, true)
}
