use gtk::{gdk, glib};
use qrcode::{Color, EcLevel, QrCode};

const QUIET_ZONE: usize = 4;
const PX_PER_MODULE: usize = 8;

/// Renders `payload` as a black-on-white QR texture (white even in dark mode,
/// so phone cameras read it reliably).
pub fn texture(payload: &str) -> Option<gdk::MemoryTexture> {
    let code = QrCode::with_error_correction_level(payload, EcLevel::M).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let size = (modules + 2 * QUIET_ZONE) * PX_PER_MODULE;
    let mut rgba = vec![255u8; size * size * 4];
    for (i, color) in colors.iter().enumerate() {
        if *color != Color::Dark {
            continue;
        }
        let (mx, my) = (i % modules + QUIET_ZONE, i / modules + QUIET_ZONE);
        for y in my * PX_PER_MODULE..(my + 1) * PX_PER_MODULE {
            for x in mx * PX_PER_MODULE..(mx + 1) * PX_PER_MODULE {
                let p = (y * size + x) * 4;
                rgba[p..p + 3].fill(0);
            }
        }
    }
    Some(gdk::MemoryTexture::new(
        size as i32,
        size as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(rgba),
        size * 4,
    ))
}
