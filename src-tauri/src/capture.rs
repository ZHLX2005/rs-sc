//! Full-screen screenshot to raw RGBA bytes in memory.
//!
//! We only support Windows + Linux here. macOS would call `screencapture` as a subprocess;
//! it is intentionally out of scope for rs-sc v1.

#[cfg(not(target_os = "macos"))]
use screenshots::Screen;

use crate::error::{AppError, AppResult};

#[cfg(not(target_os = "macos"))]
pub struct MonitorInfo {
    pub scale_factor: f64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(not(target_os = "macos"))]
pub struct CursorMonitor {
    pub screen: Screen,
    pub monitor: MonitorInfo,
}

/// Find the monitor under the cursor (Windows / Linux).
#[cfg(not(target_os = "macos"))]
pub fn find_cursor_monitor() -> AppResult<CursorMonitor> {
    let (cursor_x, cursor_y) = cursor_position()?;

    let screen = Screen::from_point(cursor_x, cursor_y)
        .map_err(|e| AppError::Capture(format!("no screen at ({cursor_x},{cursor_y}): {e}")))?;

    let info = &screen.display_info;
    Ok(CursorMonitor {
        screen,
        monitor: MonitorInfo {
            scale_factor: info.scale_factor as f64,
            x: info.x,
            y: info.y,
            width: info.width,
            height: info.height,
        },
    })
}

#[cfg(not(target_os = "macos"))]
pub fn capture_screen_rgba(screen: Screen) -> AppResult<(Vec<u8>, u32, u32)> {
    let capture = screen.capture().map_err(|e| AppError::Capture(e.to_string()))?;
    let w = capture.width();
    let h = capture.height();
    let rgba = capture.into_raw();
    Ok((rgba, w, h))
}

#[cfg(target_os = "windows")]
fn cursor_position() -> AppResult<(i32, i32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(lpPoint: *mut Point) -> i32;
    }
    let mut pt = Point { x: 0, y: 0 };
    let result = unsafe { GetCursorPos(&mut pt) };
    if result == 0 {
        return Err(AppError::Capture("GetCursorPos failed".into()));
    }
    Ok((pt.x, pt.y))
}

#[cfg(target_os = "linux")]
fn cursor_position() -> AppResult<(i32, i32)> {
    // Wayland doesn't expose global cursor. Linux mint/X11 do via libx11.
    // For v1 we just default to (0,0) which resolves to the primary screen via from_point.
    Ok((0, 0))
}

/// Crop a sub-rectangle from a contiguous RGBA buffer (row-major, row stride = img_w * 4).
pub fn crop_rgba(rgba: &[u8], img_w: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in y..(y + h) {
        let start = ((row * img_w + x) * 4) as usize;
        let end = start + (w * 4) as usize;
        if end <= rgba.len() {
            out.extend_from_slice(&rgba[start..end]);
        }
    }
    out
}

/// Encode an RGBA buffer as PNG.
pub fn encode_png(rgba: &[u8], w: u32, h: u32) -> AppResult<Vec<u8>> {
    use image::{ImageBuffer, RgbaImage};
    let img: RgbaImage = ImageBuffer::from_raw(w, h, rgba.to_vec())
        .ok_or_else(|| AppError::Capture("invalid RGBA dimensions for PNG".into()))?;
    let mut png: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| AppError::Capture(format!("PNG encode error: {e}")))?;
    Ok(png)
}
