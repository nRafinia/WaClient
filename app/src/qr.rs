//! QR payload -> slint image.

use qrcode::{Color as QrColor, QrCode};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

const TARGET_PX: usize = 264;
const QUIET_MODULES: usize = 4;

/// Renders black modules on white with a quiet zone, scaled by an integer
/// factor so the result is about 264px and stays crisp.
pub fn render(payload: &str) -> Option<Image> {
    let code = QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width();
    let total = width + 2 * QUIET_MODULES;
    let scale = (TARGET_PX / total).max(1);
    let px = total * scale;
    let colors = code.to_colors();

    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(px as u32, px as u32);
    let pixels = buf.make_mut_slice();
    let white = Rgba8Pixel::new(255, 255, 255, 255);
    let black = Rgba8Pixel::new(0, 0, 0, 255);
    for (y, row) in pixels.chunks_exact_mut(px).enumerate() {
        let my = y / scale;
        for (x, p) in row.iter_mut().enumerate() {
            let mx = x / scale;
            let dark = my >= QUIET_MODULES
                && mx >= QUIET_MODULES
                && my < QUIET_MODULES + width
                && mx < QUIET_MODULES + width
                && colors[(my - QUIET_MODULES) * width + (mx - QUIET_MODULES)] == QrColor::Dark;
            *p = if dark { black } else { white };
        }
    }
    Some(Image::from_rgba8(buf))
}
