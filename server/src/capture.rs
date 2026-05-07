use scrap::{Capturer, Display};
use std::io::ErrorKind;
use std::time::Duration;

/// Capture-backend abstraction (Block D).
///
/// Concrete implementations are:
///
/// * [`ScreenCapture`] — the existing `scrap` (DXGI on Windows / X11 on
///   Linux) backend.  Selected by `[capture].capture_backend = "scrap"`
///   (the default) and fully wired into the server today.
/// * [`WgcCapture`] — Windows.Graphics.Capture skeleton, currently a
///   `unimplemented!` stub.  Selectable per config but `unimplemented!`
///   until the real WGC integration lands in a follow-up.
///
/// The trait is intentionally minimal so a third-party can add a new
/// backend without touching the encoder pipeline.  `next_frame` returns
/// a borrowed slice into a backend-owned BGRA buffer to avoid copying
/// the full ~33 MB (4K) framebuffer per frame.
///
/// Note: the trait deliberately does *not* require `Send` — `scrap`'s
/// X11 backend holds an `Rc<xcb::Connection>` and so is `!Send` on
/// Linux.  Callers that need to move the capture across threads do so
/// today via `tokio::task::spawn_blocking`, which `move`s the value
/// into a dedicated worker thread before any trait method is invoked.
#[allow(dead_code)] // selectable via `[capture].capture_backend`; today the
                    // server still uses `ScreenCapture` directly. Trait will
                    // be the dispatch point once `WgcCapture` lands.
pub trait Capture {
    /// Width × height of the most recent capture, in pixels.
    fn dimensions(&self) -> (u32, u32);

    /// Acquire the next frame.  Returns a BGRA-packed slice of length
    /// `width * height * 4`.  Blocking; backends may return
    /// `ErrorKind::WouldBlock` when no new frame is yet available so
    /// the caller can sleep / retry without spinning.
    fn next_frame(&mut self) -> Result<&[u8], Box<dyn std::error::Error>>;
}

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

/// `Capture` impl wrapping the existing `scrap` capture path.
impl Capture for ScreenCapture {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn next_frame(&mut self) -> Result<&[u8], Box<dyn std::error::Error>> {
        self.capture_frame()
    }
}

/// Type alias for the soon-to-be-renamed `ScreenCapture`.  Block D's
/// design doc names the scrap-backed implementation `ScrapCapture`;
/// the type alias lets new code refer to it by the canonical name
/// while keeping the public API stable for existing callers.
#[allow(dead_code)]
pub type ScrapCapture = ScreenCapture;

/// Stub Windows.Graphics.Capture backend (Block D).
///
/// Selectable via `[capture].capture_backend = "wgc"` in `config.toml`,
/// but every method currently panics with `unimplemented!`.  The actual
/// integration (via the `windows` crate's
/// `Windows::Graphics::Capture::Direct3D11CaptureFramePool`) is tracked
/// as a follow-up — this skeleton lives here so the trait surface and
/// the configuration plumbing are in place ahead of that work.
#[allow(dead_code)]
pub struct WgcCapture {
    _private: (),
}

impl WgcCapture {
    #[allow(dead_code)]
    pub fn new(_monitor_index: usize) -> Result<Self, Box<dyn std::error::Error>> {
        Err("WGC capture backend is not implemented yet — \
             use `[capture].capture_backend = \"scrap\"` (the default)".into())
    }
}

impl Capture for WgcCapture {
    fn dimensions(&self) -> (u32, u32) {
        unimplemented!("WgcCapture::dimensions — pending Windows.Graphics.Capture integration")
    }
    fn next_frame(&mut self) -> Result<&[u8], Box<dyn std::error::Error>> {
        unimplemented!("WgcCapture::next_frame — pending Windows.Graphics.Capture integration")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `WgcCapture::new` must fail cleanly (not panic) so an operator
    /// who flips `[capture].capture_backend = "wgc"` ahead of the real
    /// integration gets a clear error message rather than a crash.
    #[test]
    fn wgc_capture_constructor_returns_error() {
        match WgcCapture::new(0) {
            Ok(_) => panic!("WGC stub must report not-implemented, not succeed"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("WGC") && msg.contains("not implemented"),
                    "unexpected error text: {msg}"
                );
            }
        }
    }

    /// A trait object can be built from `ScreenCapture` — pins the
    /// Block-D `Capture` trait surface so a future change can't silently
    /// break the abstraction.
    #[test]
    fn capture_trait_object_compiles() {
        // Compile-time only: we can't actually open a capturer in CI
        // (no display), so this just checks dyn-dispatch is available.
        fn _accept(_: &mut dyn Capture) {}
    }
}
