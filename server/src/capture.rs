use scrap::{Capturer, Display};
use std::io::ErrorKind;
use std::time::Duration;

/// Wraps the platform screen-capture API (DXGI on Windows).
pub struct ScreenCapture {
    capturer: Capturer,
    width: u32,
    height: u32,
}

impl ScreenCapture {
    /// Open the primary display for capture.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let display = Display::primary()?;
        let w = display.width() as u32;
        let h = display.height() as u32;
        let capturer = Capturer::new(display)?;
        log::info!("Screen capture initialized: {}×{}", w, h);
        Ok(Self { capturer, width: w, height: h })
    }

    /// Open a specific display by index.
    pub fn new_for_display(index: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let displays = Display::all()?;
        if index >= displays.len() {
            return Err(format!(
                "Monitor index {} out of range (found {} monitors)",
                index,
                displays.len()
            )
            .into());
        }
        // Re-fetch to get ownership — Display::all() returns a Vec we can consume.
        let display = displays.into_iter().nth(index).unwrap();
        let w = display.width() as u32;
        let h = display.height() as u32;
        let capturer = Capturer::new(display)?;
        log::info!("Screen capture initialized for monitor {}: {}×{}", index, w, h);
        Ok(Self { capturer, width: w, height: h })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Capture a single frame. Returns tightly-packed BGRA pixel data
    /// (stride == width × 4).
    ///
    /// On Windows/DXGI the mapped surface may have a row pitch larger
    /// than `width * 4`. We strip the padding so FFmpeg receives the
    /// exact frame size it expects.
    pub fn capture_frame(&mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        loop {
            match self.capturer.frame() {
                Ok(frame) => {
                    let expected_stride = self.width as usize * 4;
                    let expected_size = expected_stride * self.height as usize;

                    if frame.len() == expected_size {
                        // No padding – fast path.
                        return Ok(frame.to_vec());
                    }

                    // Row pitch is larger than width×4 → strip padding.
                    let actual_stride = frame.len() / self.height as usize;
                    let mut packed = Vec::with_capacity(expected_size);
                    for row in 0..self.height as usize {
                        let start = row * actual_stride;
                        packed.extend_from_slice(&frame[start..start + expected_stride]);
                    }
                    return Ok(packed);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // No new frame yet – yield briefly to avoid busy-waiting.
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Enumerate all available displays and return monitor info.
///
/// On Windows, uses Win32 `EnumDisplayMonitors` + `GetMonitorInfoW` to obtain
/// the actual virtual-desktop positions of each monitor. The results are
/// matched to `scrap::Display::all()` by index so that the indices used for
/// capture (scrap) and for input coordinate mapping (Win32) are consistent.
pub fn enumerate_monitors() -> Vec<protocol::MonitorInfo> {
    #[cfg(windows)]
    {
        enumerate_monitors_win32()
    }
    #[cfg(not(windows))]
    {
        enumerate_monitors_fallback()
    }
}

/// Win32 implementation: enumerates monitors with real positions.
#[cfg(windows)]
fn enumerate_monitors_win32() -> Vec<protocol::MonitorInfo> {
    use std::mem;
    use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};
    use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
    use winapi::um::winuser::{EnumDisplayMonitors, GetMonitorInfoW, MONITORINFO, MONITORINFOF_PRIMARY};

    /// Per-monitor data collected by the EnumDisplayMonitors callback.
    struct MonRect {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        primary: bool,
    }

    unsafe extern "system" fn callback(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: LPRECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data as *mut Vec<MonRect>);
        let mut info: MONITORINFO = mem::zeroed();
        info.cbSize = mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut info) != 0 {
            let r = info.rcMonitor;
            monitors.push(MonRect {
                x: r.left,
                y: r.top,
                w: (r.right - r.left) as u32,
                h: (r.bottom - r.top) as u32,
                primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
            });
        }
        TRUE
    }

    let mut win32_rects: Vec<MonRect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(callback),
            &mut win32_rects as *mut _ as LPARAM,
        );
    }

    // Sort: primary first, then by (x, y) so the order is deterministic and
    // closely matches the typical DXGI (scrap) enumeration order.
    win32_rects.sort_by(|a, b| {
        b.primary.cmp(&a.primary)
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });

    // Build the protocol MonitorInfo list.  We also cross-check against scrap
    // to use the same count (scrap is the source of truth for capture).
    let scrap_count = Display::all().map(|d| d.len()).unwrap_or(0);
    let count = scrap_count.min(win32_rects.len());

    (0..count)
        .map(|i| {
            let r = &win32_rects[i];
            protocol::MonitorInfo {
                index: i as u8,
                x: r.x as i16,
                y: r.y as i16,
                width: r.w as u16,
                height: r.h as u16,
                primary: r.primary,
            }
        })
        .collect()
}

/// Fallback for non-Windows: uses scrap only (no position data).
#[cfg(not(windows))]
fn enumerate_monitors_fallback() -> Vec<protocol::MonitorInfo> {
    match Display::all() {
        Ok(displays) => {
            displays
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    protocol::MonitorInfo {
                        index: i as u8,
                        x: 0,
                        y: 0,
                        width: d.width() as u16,
                        height: d.height() as u16,
                        primary: i == 0,
                    }
                })
                .collect()
        }
        Err(e) => {
            log::error!("Failed to enumerate displays: {e}");
            Vec::new()
        }
    }
}
