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
        log::info!("Screen capture initialised: {}×{}", w, h);
        Ok(Self { capturer, width: w, height: h })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Capture a single frame. Returns BGRA pixel data.
    /// Spins briefly when no new frame is available (DXGI
    /// signals `WouldBlock` until the desktop
    /// presents a new
    /// composition).
    pub fn capture_frame(&mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        loop {
            match self.capturer.frame() {
                Ok(frame) => {
                    return Ok(frame.to_vec());
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
