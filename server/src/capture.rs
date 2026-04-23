use scrap::{Capturer, Display};
use std::io::ErrorKind;
use std::time::Duration;

/// Wraps the platform screen-capture API (DXGI on Windows).
///
/// ## Mouse cursor handling
///
/// On Windows, `scrap` builds on the DXGI Desktop Duplication API
/// (`IDXGIOutputDuplication::AcquireNextFrame` + `MapDesktopSurface`,
/// see `scrap-0.5.0/src/dxgi/mod.rs:111`).  Per the DXGI contract the
/// returned desktop image **does not contain the hardware mouse
/// cursor** — Windows composites the cursor at scan-out time, and the
/// pointer shape is delivered out-of-band via `GetFramePointerShape`.
///
/// This means the encoder does not waste high-frequency bits redrawing
/// the cursor on every frame (which is the optimization Sunshine /
/// Parsec are known for), and the client-side overlay rendered at
/// `#remote-cursor` from `MSG_CURSOR_INFO` updates is the single source
/// of cursor pixels presented to the viewer.
///
/// Caveat: a small number of applications (notably some legacy games)
/// draw their own software cursor into the framebuffer; those *will*
/// appear in the captured frame.  Stripping them would require
/// detecting and masking the software-cursor region — there is no
/// general DXGI API for it.
pub struct ScreenCapture {
    capturer: Capturer,
    width: u32,
    height: u32,
    /// Persistent BGRA buffer reused across `capture_frame` calls so we
    /// don't allocate ~33 MB (4K) on every frame.  Capacity is grown to
    /// `width * height * 4` lazily on the first capture.
    buf: Vec<u8>,
}

impl ScreenCapture {
    /// Open the primary display for capture.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let display = Display::primary()?;
        let w = display.width() as u32;
        let h = display.height() as u32;
        let capturer = Capturer::new(display)?;
        log::info!("Screen capture initialized: {}×{}", w, h);
        Ok(Self { capturer, width: w, height: h, buf: Vec::new() })
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
        Ok(Self { capturer, width: w, height: h, buf: Vec::new() })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Capture a single frame. Returns tightly-packed BGRA pixel data
    /// (stride == width × 4) as a borrowed slice into a buffer owned by
    /// the capturer — valid until the next call to `capture_frame`.
    ///
    /// On Windows/DXGI the mapped surface may have a row pitch larger
    /// than `width * 4`. We strip the padding so FFmpeg receives the
    /// exact frame size it expects.
    pub fn capture_frame(&mut self) -> Result<&[u8], Box<dyn std::error::Error>> {
        loop {
            match self.capturer.frame() {
                Ok(frame) => {
                    let expected_stride = self.width as usize * 4;
                    let expected_size = expected_stride * self.height as usize;

                    // Reuse the persistent buffer; resize without
                    // reallocating once it has reached `expected_size`.
                    self.buf.clear();
                    self.buf.reserve(expected_size);

                    if frame.len() == expected_size {
                        // No padding – fast path: single copy into the
                        // reusable buffer.
                        self.buf.extend_from_slice(&frame);
                    } else {
                        // Row pitch is larger than width×4 → strip
                        // padding row by row into the persistent buffer.
                        let actual_stride = frame.len() / self.height as usize;
                        for row in 0..self.height as usize {
                            let start = row * actual_stride;
                            self.buf
                                .extend_from_slice(&frame[start..start + expected_stride]);
                        }
                    }
                    // Drop the scrap `Frame` borrow before returning a
                    // borrow into `self.buf`.
                    return Ok(&self.buf);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // No new frame yet – yield briefly to avoid
                    // busy-waiting.  500 µs is short enough to be
                    // invisible at any realistic FPS (1/60s ≈ 16.6 ms,
                    // so this is < 4 % of a frame budget) and halves
                    // the worst-case capture-side wakeup latency
                    // compared to the previous 1 ms sleep.  scrap does
                    // not expose DXGI's `AcquireNextFrame(timeout)`
                    // blocking primitive — replacing scrap with a
                    // direct DXGI binding to get true wait-on-update
                    // semantics is tracked as a separate follow-up.
                    std::thread::sleep(Duration::from_micros(500));
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
    // NOTE: If the DXGI and GDI orders diverge on a specific machine, the
    // monitor indices could be mismatched.  In practice both APIs enumerate
    // the primary display first and then secondary displays left-to-right,
    // so a sorted-by-position ordering is the best heuristic available.
    win32_rects.sort_by(|a, b| {
        b.primary.cmp(&a.primary)
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });

    // Build the protocol MonitorInfo list.  We also cross-check against scrap
    // to use the same count (scrap is the source of truth for capture).
    let scrap_count = Display::all().map(|d| d.len()).unwrap_or(0);
    if scrap_count != win32_rects.len() {
        log::warn!(
            "Monitor count mismatch: scrap reports {} display(s), Win32 reports {} — \
             using the smaller value",
            scrap_count,
            win32_rects.len()
        );
    }
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
