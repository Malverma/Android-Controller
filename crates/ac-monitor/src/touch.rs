//! Turns finger gestures on the phone into mouse actions for the PC. Pure, so
//! every gesture is tested without a phone or a compositor.

use serde_json::Value;

/// How far a finger moves (fraction of the screen's width) before a press
/// becomes a drag instead of a click.
const DRAG_THRESHOLD: f64 = 0.015;
const LONG_PRESS_MS: u64 = 600;
/// Scroll distance per fraction of the screen moved by two fingers.
const SCROLL_PER_SCREEN: f64 = 900.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Down,
    /// A finger left, others remain.
    PointerUp,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub id: u32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TouchEvent {
    pub action: TouchAction,
    pub time_ms: u64,
    pub points: Vec<Point>,
}

impl TouchEvent {
    /// `{"a":"down","t":123,"p":[[id,x,y],...]}` from `MonitorActivity`.
    pub fn from_json(v: &Value) -> Option<Self> {
        let action = match v.get("a")?.as_str()? {
            "down" => TouchAction::Down,
            "pointer_up" => TouchAction::PointerUp,
            "move" => TouchAction::Move,
            "up" => TouchAction::Up,
            "cancel" => TouchAction::Cancel,
            _ => return None,
        };
        let points = v
            .get("p")?
            .as_array()?
            .iter()
            .map(|p| {
                let p = p.as_array()?;
                Some(Point { id: p.first()?.as_u64()? as u32, x: p.get(1)?.as_f64()?, y: p.get(2)?.as_f64()? })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { action, time_ms: v.get("t")?.as_u64()?, points })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerAction {
    /// Absolute position, 0..1 of the monitor.
    Move { x: f64, y: f64 },
    Button { button: Button, pressed: bool },
    /// Positive `dy` scrolls down.
    Scroll { dy: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Gesture {
    Idle,
    /// One finger down, not yet a click or a drag.
    Pressed { x: f64, y: f64, since_ms: u64 },
    Dragging,
    Scrolling { y: f64 },
    /// More fingers came and went: ignore until all are up.
    Finished,
}

pub struct TouchMapper {
    gesture: Gesture,
}

impl Default for TouchMapper {
    fn default() -> Self {
        Self { gesture: Gesture::Idle }
    }
}

fn clamp(p: Point) -> (f64, f64) {
    (p.x.clamp(0.0, 1.0), p.y.clamp(0.0, 1.0))
}

fn mean_y(points: &[Point]) -> f64 {
    points.iter().map(|p| p.y).sum::<f64>() / points.len().max(1) as f64
}

impl TouchMapper {
    pub fn handle(&mut self, ev: &TouchEvent) -> Vec<PointerAction> {
        use Gesture as G;
        use PointerAction as A;
        use TouchAction as T;
        let first = ev.points.first().copied();
        match (self.gesture, ev.action) {
            (G::Dragging, T::Cancel) => {
                self.gesture = G::Idle;
                vec![A::Button { button: Button::Left, pressed: false }]
            }
            (_, T::Cancel) => {
                self.gesture = G::Idle;
                Vec::new()
            }
            (G::Idle, T::Down) => {
                let Some(p) = first else { return Vec::new() };
                let (x, y) = clamp(p);
                self.gesture = G::Pressed { x, y, since_ms: ev.time_ms };
                vec![A::Move { x, y }]
            }
            // A second finger: this is a scroll, not a press.
            (G::Pressed { .. }, T::Down) => {
                self.gesture = G::Scrolling { y: mean_y(&ev.points) };
                Vec::new()
            }
            (G::Pressed { x, y, .. }, T::Move) => {
                let Some(p) = first else { return Vec::new() };
                let (nx, ny) = clamp(p);
                if (nx - x).hypot(ny - y) < DRAG_THRESHOLD {
                    return Vec::new();
                }
                self.gesture = G::Dragging;
                vec![A::Button { button: Button::Left, pressed: true }, A::Move { x: nx, y: ny }]
            }
            (G::Pressed { since_ms, .. }, T::Up) => {
                self.gesture = G::Idle;
                let button = if ev.time_ms.saturating_sub(since_ms) >= LONG_PRESS_MS { Button::Right } else { Button::Left };
                vec![A::Button { button, pressed: true }, A::Button { button, pressed: false }]
            }
            (G::Dragging, T::Move) => {
                let Some(p) = first else { return Vec::new() };
                let (x, y) = clamp(p);
                vec![A::Move { x, y }]
            }
            (G::Dragging, T::Up) => {
                self.gesture = G::Idle;
                vec![A::Button { button: Button::Left, pressed: false }]
            }
            (G::Scrolling { y }, T::Move) if ev.points.len() >= 2 => {
                let now = mean_y(&ev.points);
                self.gesture = G::Scrolling { y: now };
                // Fingers moving up push the content up, like on the phone.
                vec![A::Scroll { dy: (y - now) * SCROLL_PER_SCREEN }]
            }
            (G::Scrolling { .. } | G::Finished, T::Up) => {
                self.gesture = G::Idle;
                Vec::new()
            }
            (G::Scrolling { .. }, T::PointerUp) => {
                self.gesture = G::Finished;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(action: TouchAction, t: u64, pts: &[(f64, f64)]) -> TouchEvent {
        TouchEvent {
            action,
            time_ms: t,
            points: pts.iter().enumerate().map(|(i, &(x, y))| Point { id: i as u32, x, y }).collect(),
        }
    }

    const LEFT_DOWN: PointerAction = PointerAction::Button { button: Button::Left, pressed: true };
    const LEFT_UP: PointerAction = PointerAction::Button { button: Button::Left, pressed: false };

    #[test]
    fn tap_clicks_where_it_landed() {
        let mut m = TouchMapper::default();
        assert_eq!(m.handle(&ev(TouchAction::Down, 0, &[(0.25, 0.5)])), vec![PointerAction::Move { x: 0.25, y: 0.5 }]);
        assert_eq!(m.handle(&ev(TouchAction::Up, 100, &[(0.25, 0.5)])), vec![LEFT_DOWN, LEFT_UP]);
    }

    #[test]
    fn finger_jitter_is_still_a_tap() {
        let mut m = TouchMapper::default();
        m.handle(&ev(TouchAction::Down, 0, &[(0.5, 0.5)]));
        assert!(m.handle(&ev(TouchAction::Move, 30, &[(0.505, 0.5)])).is_empty());
        assert_eq!(m.handle(&ev(TouchAction::Up, 60, &[(0.505, 0.5)])), vec![LEFT_DOWN, LEFT_UP]);
    }

    #[test]
    fn long_press_right_clicks() {
        let mut m = TouchMapper::default();
        m.handle(&ev(TouchAction::Down, 0, &[(0.5, 0.5)]));
        let got = m.handle(&ev(TouchAction::Up, 700, &[(0.5, 0.5)]));
        assert_eq!(got[0], PointerAction::Button { button: Button::Right, pressed: true });
        assert_eq!(got[1], PointerAction::Button { button: Button::Right, pressed: false });
    }

    #[test]
    fn moving_drags_and_up_releases() {
        let mut m = TouchMapper::default();
        m.handle(&ev(TouchAction::Down, 0, &[(0.1, 0.1)]));
        assert_eq!(m.handle(&ev(TouchAction::Move, 20, &[(0.3, 0.1)])), vec![LEFT_DOWN, PointerAction::Move { x: 0.3, y: 0.1 }]);
        assert_eq!(m.handle(&ev(TouchAction::Move, 40, &[(0.4, 0.2)])), vec![PointerAction::Move { x: 0.4, y: 0.2 }]);
        assert_eq!(m.handle(&ev(TouchAction::Up, 60, &[(0.4, 0.2)])), vec![LEFT_UP]);
    }

    #[test]
    fn cancel_during_a_drag_never_leaves_the_button_held() {
        let mut m = TouchMapper::default();
        m.handle(&ev(TouchAction::Down, 0, &[(0.1, 0.1)]));
        m.handle(&ev(TouchAction::Move, 20, &[(0.3, 0.1)]));
        assert_eq!(m.handle(&ev(TouchAction::Cancel, 30, &[(0.3, 0.1)])), vec![LEFT_UP]);
        assert_eq!(m.handle(&ev(TouchAction::Move, 40, &[(0.5, 0.1)])), vec![], "gesture is over");
    }

    #[test]
    fn two_fingers_scroll_and_never_click() {
        let mut m = TouchMapper::default();
        m.handle(&ev(TouchAction::Down, 0, &[(0.4, 0.5)]));
        assert!(m.handle(&ev(TouchAction::Down, 10, &[(0.4, 0.5), (0.6, 0.5)])).is_empty());
        let got = m.handle(&ev(TouchAction::Move, 30, &[(0.4, 0.4), (0.6, 0.4)]));
        let [PointerAction::Scroll { dy }] = got[..] else { panic!("{got:?}") };
        assert!(dy > 0.0, "fingers up scrolls down: {dy}");
        assert!(m.handle(&ev(TouchAction::PointerUp, 40, &[(0.4, 0.4), (0.6, 0.4)])).is_empty());
        // The remaining finger neither drags nor clicks.
        assert!(m.handle(&ev(TouchAction::Move, 50, &[(0.2, 0.2)])).is_empty());
        assert!(m.handle(&ev(TouchAction::Up, 60, &[(0.2, 0.2)])).is_empty());
        assert_eq!(m.handle(&ev(TouchAction::Down, 70, &[(0.9, 0.9)])), vec![PointerAction::Move { x: 0.9, y: 0.9 }]);
    }

    #[test]
    fn positions_stay_on_the_monitor() {
        let mut m = TouchMapper::default();
        assert_eq!(m.handle(&ev(TouchAction::Down, 0, &[(-0.2, 1.4)])), vec![PointerAction::Move { x: 0.0, y: 1.0 }]);
    }

    #[test]
    fn reads_the_helpers_json() {
        let e = TouchEvent::from_json(&json!({"op":"touch","a":"down","t":99,"p":[[3,0.5,0.25]]})).unwrap();
        assert_eq!((e.action, e.time_ms), (TouchAction::Down, 99));
        assert_eq!(e.points, vec![Point { id: 3, x: 0.5, y: 0.25 }]);
        assert!(TouchEvent::from_json(&json!({"a":"down","t":1,"p":[[1,"x",0]]})).is_none());
    }
}
