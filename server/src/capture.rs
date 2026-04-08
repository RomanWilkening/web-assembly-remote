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
pub fn enumerate_monitors() -> Vec<protocol::MonitorInfo> {
    match Display::all() {
        Ok(displays) => {
            let primary = Display::primary().ok();
            let primary_dims = primary.as_ref().map(|d| (d.width(), d.height()));

            displays
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    // scrap doesn't expose position, so we use index-based identification.
                    // Primary detection is approximate.
                    let is_primary = match &primary_dims {
                        Some((pw, ph)) if i == 0 => d.width() == *pw && d.height() == *ph,
                        _ => i == 0,
                    };
                    protocol::MonitorInfo {
                        index: i as u8,
                        x: 0,   // scrap doesn't expose monitor offset
                        y: 0,
                        width: d.width() as u16,
                        height: d.height() as u16,
                        primary: is_primary,
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
