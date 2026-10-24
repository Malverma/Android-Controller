//! `GridLayoutManager` (spec §6.1): picks the rows × columns arrangement that
//! shows the most phone, and lays the slot cards out in it.
//!
//! Every shape with `rows · cols ≥ N` is scored by the summed area of each
//! phone's frame letterboxed (`CONTAIN`) into its tile, minus the card's own
//! chrome. The best shape wins; ties go to fewer rows. Empty slots collapse
//! into a narrow column on the right so phones get the space. A change of
//! shape animates over 200 ms (ease-out cubic).

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Space between tiles, in logical pixels.
pub const GAP: f64 = 12.0;
/// Width of the column holding empty slots.
pub const COMPACT_W: f64 = 160.0;
/// Frame assumed for a tile that has no video yet (a 20:9 portrait phone).
pub const DEFAULT_FRAME: (u32, u32) = (1080, 2400);
/// Card space around the mirror until a real allocation can be measured.
const DEFAULT_CHROME: (f64, f64) = (26.0, 170.0);
const ANIM_US: i64 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub rows: u32,
    pub cols: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub width: f64,
    pub height: f64,
    pub gap: f64,
    /// Card space that is not mirror: `(horizontal, vertical)`.
    pub chrome: (f64, f64),
    /// Smallest tile a card can be allocated.
    pub min_tile: (f64, f64),
}

/// Tile size for a shape: `((W − gaps) / cols, (H − gaps) / rows)`.
pub fn tile_size(p: &Params, s: Shape) -> (f64, f64) {
    let w = (p.width - p.gap * (s.cols as f64 - 1.0)) / s.cols as f64;
    let h = (p.height - p.gap * (s.rows as f64 - 1.0)) / s.rows as f64;
    (w.max(0.0), h.max(0.0))
}

/// Area of a `frame` letterboxed into `w × h`.
pub fn contained_area(frame: (u32, u32), w: f64, h: f64) -> f64 {
    if w <= 0.0 || h <= 0.0 || frame.0 == 0 || frame.1 == 0 {
        return 0.0;
    }
    let (fw, fh) = (frame.0 as f64, frame.1 as f64);
    let s = (w / fw).min(h / fh);
    s * fw * s * fh
}

/// Columns of at least `min_w` that fit in `width`, in `1..=n`.
pub fn cols_that_fit(width: f64, min_w: f64, gap: f64, n: u32) -> u32 {
    let fit = ((width + gap) / (min_w + gap).max(1.0)).floor() as u32;
    fit.clamp(1, n.max(1))
}

/// The best shape for these frames (spec §6.1). Shapes whose tiles would be
/// smaller than `min_tile` are skipped; if none fits, the widest that fits
/// horizontally is used (the container then scrolls vertically).
pub fn choose(frames: &[(u32, u32)], p: &Params) -> Shape {
    let n = frames.len().max(1) as u32;
    let mut best: Option<(f64, Shape)> = None;
    // Ascending columns means descending rows, so `>=` lets the later
    // (fewer-rows) shape win a tie.
    for cols in 1..=n {
        let shape = Shape { rows: n.div_ceil(cols), cols };
        let (tw, th) = tile_size(p, shape);
        if tw + 0.5 < p.min_tile.0 || th + 0.5 < p.min_tile.1 {
            continue;
        }
        let (pw, ph) = (tw - p.chrome.0, th - p.chrome.1);
        let score: f64 = frames.iter().map(|&f| contained_area(f, pw, ph)).sum();
        if best.is_none_or(|(b, _)| score >= b * (1.0 - 1e-9)) {
            best = Some((score, shape));
        }
    }
    best.map(|(_, s)| s).unwrap_or_else(|| {
        let cols = cols_that_fit(p.width, p.min_tile.0, p.gap, n);
        Shape { rows: n.div_ceil(cols), cols }
    })
}

/// What the layout needs to know about one slot card, in child order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileInfo {
    /// Current capture size, or `None` for no video (assumes [`DEFAULT_FRAME`]).
    pub frame: Option<(u32, u32)>,
    /// An empty slot: goes in the narrow column.
    pub compact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn lerp(a: Rect, b: Rect, t: f64) -> Rect {
        let l = |a: f64, b: f64| a + (b - a) * t;
        Rect { x: l(a.x, b.x), y: l(a.y, b.y), w: l(a.w, b.w), h: l(a.h, b.h) }
    }
}

/// Identifies an arrangement; a change starts the relayout animation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutKey {
    shape: Shape,
    regular: Vec<usize>,
    compact: Vec<usize>,
}

struct Anim {
    from: Vec<Option<Rect>>,
    start_us: i64,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GridLayout {
        pub tiles: RefCell<Vec<TileInfo>>,
        pub(super) last: RefCell<Vec<Option<Rect>>>,
        pub(super) last_key: RefCell<Option<LayoutKey>>,
        pub(super) anim: RefCell<Option<Anim>>,
        pub(super) ticking: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GridLayout {
        const NAME: &'static str = "AcGridLayout";
        type Type = super::GridLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for GridLayout {}

    impl LayoutManagerImpl for GridLayout {
        fn request_mode(&self, _widget: &gtk::Widget) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, widget: &gtk::Widget, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let kids = self.partition(widget);
            let (min_w, min_h) = min_tile(&kids.regular);
            let col_w = kids.compact_width();
            let side = if col_w > 0.0 { col_w + GAP } else { 0.0 };
            let n = kids.regular.len() as u32;
            match orientation {
                gtk::Orientation::Horizontal => {
                    let min = if n == 0 { col_w } else { min_w + side };
                    let nat = if n == 0 { col_w } else { (min_w + GAP) * n as f64 - GAP + side };
                    (min.ceil() as i32, nat.ceil() as i32, -1, -1)
                }
                _ => {
                    let width = if for_size >= 0 { for_size as f64 } else { (min_w + GAP) * n as f64 - GAP + side };
                    let rows = if n == 0 { 0 } else { n.div_ceil(cols_that_fit(width - side, min_w, GAP, n)) };
                    let grid = if rows == 0 { 0.0 } else { rows as f64 * (min_h + GAP) - GAP };
                    let min = grid.max(kids.compact_height(col_w));
                    (min.ceil() as i32, min.ceil() as i32, -1, -1)
                }
            }
        }

        fn allocate(&self, widget: &gtk::Widget, width: i32, height: i32, _baseline: i32) {
            let kids = self.partition(widget);
            let total = kids.count;
            let col_w = kids.compact_width();
            let side = if col_w > 0.0 { col_w + GAP } else { 0.0 };
            let (width, height) = (width as f64, height as f64);
            let mut target: Vec<Option<Rect>> = vec![None; total];

            let frames: Vec<(u32, u32)> = kids.regular.iter().map(|(_, _, t)| t.frame.unwrap_or(DEFAULT_FRAME)).collect();
            let params = Params {
                width: width - side,
                height,
                gap: GAP,
                chrome: chrome(&kids.regular),
                min_tile: min_tile(&kids.regular),
            };
            let shape = choose(&frames, &params);
            let (tw, th) = tile_size(&params, shape);
            let (tw, th) = (tw.max(params.min_tile.0).floor(), th.max(params.min_tile.1).floor());
            let n = kids.regular.len();
            for (k, (i, _, _)) in kids.regular.iter().enumerate() {
                let (row, col) = (k / shape.cols as usize, k % shape.cols as usize);
                // Center a partly filled last row.
                let in_row = (n - row * shape.cols as usize).min(shape.cols as usize);
                let row_w = in_row as f64 * (tw + GAP) - GAP;
                let x0 = ((width - side) - row_w).max(0.0) / 2.0;
                target[*i] = Some(Rect { x: x0 + col as f64 * (tw + GAP), y: row as f64 * (th + GAP), w: tw, h: th });
            }
            let mut y = 0.0;
            for (i, child) in &kids.compact {
                let h = child.measure(gtk::Orientation::Vertical, col_w as i32).0 as f64;
                target[*i] = Some(Rect { x: width - col_w, y, w: col_w, h });
                y += h + GAP;
            }

            let key = LayoutKey {
                shape,
                regular: kids.regular.iter().map(|(i, _, _)| *i).collect(),
                compact: kids.compact.iter().map(|(i, _)| *i).collect(),
            };
            let now = glib::monotonic_time();
            let animate = widget.settings().is_gtk_enable_animations() && widget.is_mapped();
            {
                let mut last_key = self.last_key.borrow_mut();
                if last_key.as_ref().is_some_and(|k| *k != key) && animate {
                    let from = self.last.borrow().clone();
                    *self.anim.borrow_mut() = Some(Anim { from, start_us: now });
                    self.start_ticking(widget);
                }
                *last_key = Some(key);
            }
            let mut anim = self.anim.borrow_mut();
            let t = anim.as_ref().map(|a| ((now - a.start_us) as f64 / ANIM_US as f64).clamp(0.0, 1.0));
            if t.is_some_and(|t| t >= 1.0) {
                *anim = None;
            }
            let ease = t.map(|t| 1.0 - (1.0 - t).powi(3));

            let mut placed = vec![None; total];
            for (i, child) in kids.all.iter().enumerate() {
                let Some(dst) = target[i] else { continue };
                let r = match (anim.as_ref(), ease) {
                    (Some(a), Some(e)) => match a.from.get(i).copied().flatten() {
                        Some(src) => Rect::lerp(src, dst, e),
                        None => dst,
                    },
                    _ => dst,
                };
                // Never below the child's minimum (GTK warns and clips).
                let w = (r.w.round() as i32).max(child.measure(gtk::Orientation::Horizontal, -1).0);
                let h = (r.h.round() as i32).max(child.measure(gtk::Orientation::Vertical, w).0);
                child.size_allocate(&gtk::Allocation::new(r.x.round() as i32, r.y.round() as i32, w, h), -1);
                placed[i] = Some(r);
            }
            *self.last.borrow_mut() = placed;
        }
    }

    pub(super) struct Kids {
        /// Every child in order (index = slot - 1), laid out or not.
        pub all: Vec<gtk::Widget>,
        pub count: usize,
        pub regular: Vec<(usize, gtk::Widget, TileInfo)>,
        pub compact: Vec<(usize, gtk::Widget)>,
    }

    impl Kids {
        pub fn compact_width(&self) -> f64 {
            self.compact
                .iter()
                .map(|(_, c)| c.measure(gtk::Orientation::Horizontal, -1).0 as f64)
                .fold(if self.compact.is_empty() { 0.0 } else { COMPACT_W }, f64::max)
        }

        pub fn compact_height(&self, col_w: f64) -> f64 {
            let sum: f64 = self.compact.iter().map(|(_, c)| c.measure(gtk::Orientation::Vertical, col_w as i32).0 as f64 + GAP).sum();
            (sum - GAP).max(0.0)
        }
    }

    impl GridLayout {
        pub(super) fn partition(&self, widget: &gtk::Widget) -> Kids {
            let tiles = self.tiles.borrow();
            let mut all = Vec::new();
            let mut child = widget.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                all.push(c);
            }
            let info = |i: usize| tiles.get(i).copied().unwrap_or(TileInfo { frame: None, compact: false });
            let shown: Vec<usize> = (0..all.len()).filter(|&i| all[i].should_layout()).collect();
            let any_regular = shown.iter().any(|&i| !info(i).compact);
            let mut regular = Vec::new();
            let mut compact = Vec::new();
            for i in shown {
                let t = info(i);
                // With no phone at all, empty slots fill the grid instead.
                if t.compact && any_regular {
                    compact.push((i, all[i].clone()));
                } else {
                    regular.push((i, all[i].clone(), t));
                }
            }
            Kids { count: all.len(), all, regular, compact }
        }

        fn start_ticking(&self, widget: &gtk::Widget) {
            if self.ticking.replace(true) {
                return;
            }
            widget.add_tick_callback(|w, _| {
                w.queue_allocate();
                let Some(layout) = w.layout_manager().and_downcast::<super::GridLayout>() else {
                    return glib::ControlFlow::Break;
                };
                let imp = layout.imp();
                if imp.anim.borrow().is_some() {
                    glib::ControlFlow::Continue
                } else {
                    imp.ticking.set(false);
                    glib::ControlFlow::Break
                }
            });
        }
    }

    /// The largest minimum size among the phone cards.
    fn min_tile(regular: &[(usize, gtk::Widget, TileInfo)]) -> (f64, f64) {
        regular.iter().fold((0.0, 0.0), |(w, h), (_, c, _)| {
            let mw = c.measure(gtk::Orientation::Horizontal, -1).0 as f64;
            let mh = c.measure(gtk::Orientation::Vertical, -1).0 as f64;
            (f64::max(w, mw), f64::max(h, mh))
        })
    }

    /// Card space around the mirror, measured from a card showing video.
    fn chrome(regular: &[(usize, gtk::Widget, TileInfo)]) -> (f64, f64) {
        for (_, card, _) in regular {
            let Some(pic) = find_mirror(card) else { continue };
            let (cw, ch, pw, ph) = (card.width(), card.height(), pic.width(), pic.height());
            if pic.is_mapped() && pw > 0 && ph > 0 && cw > pw && ch > ph {
                return ((cw - pw) as f64, (ch - ph) as f64);
            }
        }
        DEFAULT_CHROME
    }

    /// The card's mirror widget (tagged with the `mirror` CSS class).
    fn find_mirror(w: &gtk::Widget) -> Option<gtk::Widget> {
        let mut child = w.first_child();
        while let Some(c) = child {
            if c.has_css_class("mirror") {
                return Some(c);
            }
            if let Some(found) = find_mirror(&c) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        None
    }
}

glib::wrapper! {
    pub struct GridLayout(ObjectSubclass<imp::GridLayout>) @extends gtk::LayoutManager;
}

impl Default for GridLayout {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl GridLayout {
    /// Updates what the layout knows about each card; relayouts on change.
    pub fn set_tiles(&self, tiles: Vec<TileInfo>) {
        if *self.imp().tiles.borrow() == tiles {
            return;
        }
        *self.imp().tiles.borrow_mut() = tiles;
        self.layout_changed();
    }
}

mod container {
    use super::*;

    #[derive(Default)]
    pub struct Grid;

    #[glib::object_subclass]
    impl ObjectSubclass for Grid {
        const NAME: &'static str = "AcGrid";
        type Type = super::Grid;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<GridLayout>();
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for Grid {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Grid {}
}

glib::wrapper! {
    /// The container for the slot cards, laid out by [`GridLayout`].
    pub struct Grid(ObjectSubclass<container::Grid>) @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Grid {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Grid {
    pub fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
    }

    pub fn grid_layout(&self) -> GridLayout {
        self.layout_manager().and_downcast().expect("AcGrid uses GridLayout")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTRAIT: (u32, u32) = (1080, 2400);
    const TALL: (u32, u32) = (1440, 3200);
    const LANDSCAPE: (u32, u32) = (2400, 1080);
    const FOLD: (u32, u32) = (2208, 1840);

    fn params(w: f64, h: f64) -> Params {
        Params { width: w, height: h, gap: GAP, chrome: DEFAULT_CHROME, min_tile: (150.0, 300.0) }
    }

    fn pick(w: f64, h: f64, frames: &[(u32, u32)]) -> (u32, u32) {
        let s = choose(frames, &params(w, h));
        (s.rows, s.cols)
    }

    #[test]
    fn five_portrait_phones() {
        let five = [PORTRAIT; 5];
        assert_eq!(pick(1280.0, 720.0, &five), (1, 5));
        assert_eq!(pick(1920.0, 1080.0, &five), (1, 5), "the spec §6.1 wireframe");
        assert_eq!(pick(2560.0, 1440.0, &five), (1, 5));
        assert_eq!(pick(3440.0, 1440.0, &five), (1, 5));
        assert_eq!(pick(1080.0, 1920.0, &five), (2, 3), "portrait monitor");
    }

    #[test]
    fn five_landscape_phones() {
        let five = [LANDSCAPE; 5];
        assert_eq!(pick(1280.0, 720.0, &five), (2, 3));
        assert_eq!(pick(1920.0, 1080.0, &five), (2, 3));
        assert_eq!(pick(2560.0, 1440.0, &five), (2, 3));
        assert_eq!(pick(3440.0, 1440.0, &five), (2, 3));
        assert_eq!(pick(1080.0, 1920.0, &five), (3, 2), "portrait monitor");
    }

    #[test]
    fn mixed_phones() {
        let mixed = [PORTRAIT, TALL, LANDSCAPE, FOLD, PORTRAIT];
        assert_eq!(pick(1280.0, 720.0, &mixed), (1, 5));
        assert_eq!(pick(1920.0, 1080.0, &mixed), (1, 5));
        assert_eq!(pick(2560.0, 1440.0, &mixed), (1, 5));
        assert_eq!(pick(3440.0, 1440.0, &mixed), (1, 5));
        assert_eq!(pick(1080.0, 1920.0, &mixed), (2, 3));
    }

    #[test]
    fn fewer_phones() {
        assert_eq!(pick(1920.0, 1080.0, &[PORTRAIT]), (1, 1));
        assert_eq!(pick(1920.0, 1080.0, &[PORTRAIT; 2]), (1, 2));
        assert_eq!(pick(1920.0, 1080.0, &[LANDSCAPE; 4]), (2, 2));
        assert_eq!(pick(1080.0, 1920.0, &[LANDSCAPE; 2]), (2, 1));
    }

    #[test]
    fn ties_go_to_fewer_rows() {
        // Square frames in a square window: 1×2 and 2×1 tiles are the same
        // shape rotated, so their scores tie.
        let p = Params { width: 1000.0, height: 1000.0, gap: 0.0, chrome: (0.0, 0.0), min_tile: (0.0, 0.0) };
        let s = choose(&[(100, 100); 2], &p);
        assert_eq!((s.rows, s.cols), (1, 2));
    }

    #[test]
    fn tiles_below_minimum_are_skipped() {
        // 5 × 300 px tiles do not fit in 1280: falls back to fewer columns.
        let p = Params { min_tile: (300.0, 300.0), ..params(1280.0, 720.0) };
        let s = choose(&[PORTRAIT; 5], &p);
        assert!(s.cols <= 4, "{s:?}");
        // Nothing fits: the widest arrangement that fits horizontally.
        let p = Params { min_tile: (300.0, 500.0), ..params(1280.0, 400.0) };
        assert_eq!(choose(&[PORTRAIT; 5], &p), Shape { rows: 2, cols: 4 });
    }

    #[test]
    fn geometry_helpers() {
        let p = params(1920.0, 1080.0);
        assert_eq!(tile_size(&p, Shape { rows: 1, cols: 5 }), ((1920.0 - 48.0) / 5.0, 1080.0));
        assert_eq!(contained_area((100, 200), 100.0, 100.0), 50.0 * 100.0);
        assert_eq!(contained_area((100, 200), 0.0, 100.0), 0.0);
        assert_eq!(cols_that_fit(1000.0, 150.0, 12.0, 5), 5);
        assert_eq!(cols_that_fit(320.0, 150.0, 12.0, 5), 2);
        assert_eq!(cols_that_fit(10.0, 150.0, 12.0, 5), 1);
    }
}
