//! Broadcast normalization (spec §4.5): a point on the source phone lands in
//! the same *relative* spot on every target, whatever its resolution.

/// Maps device pixel `(x, y)` on a `src` frame to the matching pixel on a
/// `dst` frame: `x_t = round(x / Wf · Wf_t)`, clamped to the frame.
pub fn remap(x: i32, y: i32, src: (u32, u32), dst: (u32, u32)) -> (i32, i32) {
    if src == dst || src.0 == 0 || src.1 == 0 || dst.0 == 0 || dst.1 == 0 {
        return (x, y);
    }
    let axis = |v: i32, s: u32, d: u32| {
        let u = v as f64 / s as f64;
        (u * d as f64).round().clamp(0.0, (d - 1) as f64) as i32
    };
    (axis(x, src.0, dst.0), axis(y, src.1, dst.1))
}

/// True when one frame is landscape and the other portrait (spec §4.5:
/// such targets get a warning badge).
pub fn orientation_differs(a: (u32, u32), b: (u32, u32)) -> bool {
    (a.0 > a.1) != (b.0 > b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_is_identity() {
        assert_eq!(remap(123, 456, (1080, 2400), (1080, 2400)), (123, 456));
    }

    #[test]
    fn scales_to_a_larger_phone() {
        // 1080×2400 → 1440×3200: exactly 4/3 on both axes.
        assert_eq!(remap(540, 1200, (1080, 2400), (1440, 3200)), (720, 1600), "center");
        assert_eq!(remap(0, 0, (1080, 2400), (1440, 3200)), (0, 0));
        assert_eq!(remap(810, 2100, (1080, 2400), (1440, 3200)), (1080, 2800), "bottom-right button");
    }

    #[test]
    fn different_aspect_keeps_relative_position() {
        // 1080×2400 (20:9) → 1080×1920 (16:9): x unchanged, y scaled by 0.8.
        assert_eq!(remap(300, 2000, (1080, 2400), (1080, 1920)), (300, 1600));
        // Foldable inner screen.
        assert_eq!(remap(540, 1200, (1080, 2400), (2208, 1840)), (1104, 920));
    }

    #[test]
    fn last_pixel_stays_inside() {
        assert_eq!(remap(1079, 2399, (1080, 2400), (720, 1600)), (719, 1599));
        assert_eq!(remap(1079, 2399, (1080, 2400), (1440, 3200)), (1439, 3199));
    }

    #[test]
    fn degenerate_sizes_pass_through() {
        assert_eq!(remap(5, 6, (0, 0), (100, 100)), (5, 6));
        assert_eq!(remap(5, 6, (100, 100), (0, 100)), (5, 6));
    }

    #[test]
    fn orientation() {
        assert!(!orientation_differs((1080, 2400), (1440, 3200)));
        assert!(orientation_differs((1080, 2400), (2400, 1080)));
        assert!(!orientation_differs((2400, 1080), (2208, 1840)));
    }
}
