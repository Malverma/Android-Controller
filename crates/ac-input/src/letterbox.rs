//! Widget ↔ device coordinates for a `CONTAIN`-fitted frame (spec §4.5).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    pub frame_w: u32,
    pub frame_h: u32,
    pub scale: f64,
    pub ox: f64,
    pub oy: f64,
}

impl Letterbox {
    /// `view_*` is the widget's allocated size in logical pixels; `frame_*`
    /// is the size from the latest session packet. `None` for empty sizes.
    pub fn new(view_w: f64, view_h: f64, frame_w: u32, frame_h: u32) -> Option<Self> {
        if view_w <= 0.0 || view_h <= 0.0 || frame_w == 0 || frame_h == 0 {
            return None;
        }
        let scale = (view_w / frame_w as f64).min(view_h / frame_h as f64);
        Some(Self {
            frame_w,
            frame_h,
            scale,
            ox: (view_w - scale * frame_w as f64) / 2.0,
            oy: (view_h - scale * frame_h as f64) / 2.0,
        })
    }

    fn unscaled(&self, px: f64, py: f64) -> (f64, f64) {
        ((px - self.ox) / self.scale, (py - self.oy) / self.scale)
    }

    /// Device pixel under the pointer, or `None` in the letterbox bars.
    pub fn map(&self, px: f64, py: f64) -> Option<(i32, i32)> {
        let (fx, fy) = self.unscaled(px, py);
        let inside = (0.0..self.frame_w as f64).contains(&fx) && (0.0..self.frame_h as f64).contains(&fy);
        inside.then(|| self.clamp(fx, fy))
    }

    /// Like `map` but clamped to the frame edge: used for drags that started
    /// inside and overshoot the frame.
    pub fn map_clamped(&self, px: f64, py: f64) -> (i32, i32) {
        let (fx, fy) = self.unscaled(px, py);
        self.clamp(fx, fy)
    }

    fn clamp(&self, fx: f64, fy: f64) -> (i32, i32) {
        let x = fx.round().clamp(0.0, (self.frame_w - 1) as f64) as i32;
        let y = fy.round().clamp(0.0, (self.frame_h - 1) as f64) as i32;
        (x, y)
    }

    /// The frame's on-screen rectangle `(x, y, w, h)` in widget coordinates.
    pub fn rect(&self) -> (f64, f64, f64, f64) {
        (self.ox, self.oy, self.scale * self.frame_w as f64, self.scale * self.frame_h as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_in_landscape_tile() {
        // 1080x2400 phone in a 800x600 view: pillarboxed.
        let l = Letterbox::new(800.0, 600.0, 1080, 2400).unwrap();
        assert!((l.scale - 0.25).abs() < 1e-9);
        assert_eq!((l.ox, l.oy), (265.0, 0.0));
        assert_eq!(l.map(400.0, 300.0), Some((540, 1200)), "center maps to center");
        assert_eq!(l.map(100.0, 300.0), None, "left bar");
        assert_eq!(l.map_clamped(100.0, 300.0), (0, 1200));
        assert_eq!(l.map(265.0, 0.0), Some((0, 0)));
        assert_eq!(l.map(534.99, 599.99), Some((1079, 2399)), "bottom-right clamps to last pixel");
    }

    #[test]
    fn landscape_in_portrait_tile() {
        let l = Letterbox::new(400.0, 800.0, 2400, 1080).unwrap();
        assert!((l.scale - 400.0 / 2400.0).abs() < 1e-9);
        assert_eq!(l.ox, 0.0);
        assert!((l.oy - 310.0).abs() < 1e-9);
        assert_eq!(l.map(200.0, 400.0), Some((1200, 540)));
        assert_eq!(l.map(200.0, 100.0), None, "top bar");
    }

    #[test]
    fn exact_fit_and_tiny_tiles() {
        let l = Letterbox::new(360.0, 720.0, 360, 720).unwrap();
        assert_eq!((l.scale, l.ox, l.oy), (1.0, 0.0, 0.0));
        assert_eq!(l.map(10.4, 20.6), Some((10, 21)));
        let tiny = Letterbox::new(1.0, 1.0, 1080, 2400).unwrap();
        assert_eq!(tiny.map_clamped(0.5, 0.5), (540, 1200));
        assert!(Letterbox::new(0.0, 10.0, 10, 10).is_none());
        assert!(Letterbox::new(10.0, 10.0, 0, 10).is_none());
    }

    #[test]
    fn rect() {
        let l = Letterbox::new(800.0, 600.0, 1080, 2400).unwrap();
        assert_eq!(l.rect(), (265.0, 0.0, 270.0, 600.0));
    }
}
