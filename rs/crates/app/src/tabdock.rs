//! Tab-bar docking (t25, #85): where the tab strip lives (top, or a vertical column on the
//! right/left), the grip-drag hit test that picks a side, the strip hit test generalised to
//! either axis, and wheel-to-tab-steps. Pure logic only — the controller in `app.rs` feeds
//! it window-logical pixels and applies the results.

/// Which edge the tab bar is docked to. Per window; persisted as `tabDock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabDock {
    #[default]
    Top,
    Right,
    Left,
}

/// Width of the LEFT/RIGHT drop zones measured from the window edge.
pub const DOCK_EDGE_ZONE: f32 = 160.0;
/// Height of the top bar (the TOP drop zone is `y < TOP_ZONE`).
pub const TOP_ZONE: f32 = 32.0;
/// A press only becomes a drag after the pointer moves this far.
pub const DOCK_DRAG_THRESHOLD: f32 = 4.0;

impl TabDock {
    pub fn to_i32(self) -> i32 {
        match self {
            TabDock::Top => 0,
            TabDock::Right => 1,
            TabDock::Left => 2,
        }
    }

    #[cfg(test)]
    pub fn from_i32(v: i32) -> Option<TabDock> {
        match v {
            0 => Some(TabDock::Top),
            1 => Some(TabDock::Right),
            2 => Some(TabDock::Left),
            _ => None,
        }
    }

    /// The snapshot value: `None` for top (the field is omitted).
    pub fn persisted(self) -> Option<String> {
        match self {
            TabDock::Top => None,
            TabDock::Right => Some("right".into()),
            TabDock::Left => Some("left".into()),
        }
    }

    /// Parse a persisted value; absent or unknown → top.
    pub fn from_persisted(v: Option<&str>) -> TabDock {
        match v {
            Some("right") => TabDock::Right,
            Some("left") => TabDock::Left,
            _ => TabDock::Top,
        }
    }

    /// Top → Right → Left → Top.
    pub fn cycle(self) -> TabDock {
        match self {
            TabDock::Top => TabDock::Right,
            TabDock::Right => TabDock::Left,
            TabDock::Left => TabDock::Top,
        }
    }

    pub fn is_docked(self) -> bool {
        self != TabDock::Top
    }
}

/// The dock positions other than `current`, each with its menu/palette label.
pub fn dock_choices(current: TabDock) -> Vec<(&'static str, TabDock)> {
    [
        ("Tab bar: move to top", TabDock::Top),
        ("Tab bar: dock right", TabDock::Right),
        ("Tab bar: dock left", TabDock::Left),
    ]
    .into_iter()
    .filter(|&(_, d)| d != current)
    .collect()
}

/// Which side a release at window-logical `(x, y)` would dock to, given the window size
/// `(w, h)`. The three zones are disjoint, and the current side's own zone yields no target,
/// so the result is never `current`.
pub fn dock_target(current: TabDock, (x, y): (f32, f32), (w, _h): (f32, f32)) -> Option<TabDock> {
    let t = if y < TOP_ZONE {
        TabDock::Top
    } else if x <= DOCK_EDGE_ZONE {
        TabDock::Left
    } else if x >= w - DOCK_EDGE_ZONE {
        TabDock::Right
    } else {
        return None;
    };
    (t != current).then_some(t)
}

/// An in-flight grip drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockDrag {
    origin: (f32, f32),
    /// Past the 4 px threshold.
    pub active: bool,
    pub target: Option<TabDock>,
}

impl DockDrag {
    pub fn begin(p: (f32, f32)) -> Self {
        DockDrag {
            origin: p,
            active: false,
            target: None,
        }
    }

    /// Feed a pointer move. Returns true when `target` changed (or the drag just activated).
    pub fn moved(&mut self, p: (f32, f32), current: TabDock, win_size: (f32, f32)) -> bool {
        let was_active = self.active;
        if !self.active {
            let (dx, dy) = (p.0 - self.origin.0, p.1 - self.origin.1);
            if dx * dx + dy * dy >= DOCK_DRAG_THRESHOLD * DOCK_DRAG_THRESHOLD {
                self.active = true;
            }
        }
        let new = if self.active {
            dock_target(current, p, win_size)
        } else {
            None
        };
        let changed = new != self.target || was_active != self.active;
        self.target = new;
        changed
    }

    /// Release: the side to commit, only if the drag was active and a target exists.
    pub fn end(self) -> Option<TabDock> {
        if self.active {
            self.target
        } else {
            None
        }
    }
}

/// Hit-test a point against the per-chip `(pos, len)` geometry along the strip's axis (x for
/// top, y when docked). Returns `(insertion slot, chip under the pointer)`: the slot counts
/// chips whose centre the pointer has passed.
pub fn strip_hit(dock: TabDock, geom: &[(f32, f32)], (x, y): (f32, f32)) -> (usize, Option<usize>) {
    let p = if dock.is_docked() { y } else { x };
    let mut slot = 0;
    let mut over = None;
    for (i, &(pos, len)) in geom.iter().enumerate() {
        if p >= pos + len / 2.0 {
            slot = i + 1;
        }
        if p >= pos && p < pos + len {
            over = Some(i);
        }
    }
    (slot, over)
}

/// Is the window-logical point over the tab strip? Top: the top bar. Docked: the column
/// `col = (window-x, width)` below the top bar.
pub fn over_strip(dock: TabDock, (x, y): (f32, f32), col: (f32, f32)) -> bool {
    match dock {
        TabDock::Top => y < TOP_ZONE,
        _ => x >= col.0 && x < col.0 + col.1 && y >= TOP_ZONE,
    }
}

/// Window-x of the pane area's left edge (pane rects are area-relative): the left column's
/// right edge when docked left, else 0.
pub fn area_origin_x(dock: TabDock, col: (f32, f32)) -> f32 {
    if dock == TabDock::Left {
        col.0 + col.1
    } else {
        0.0
    }
}

/// Turns wheel deltas into tab steps. +1 = next tab (wheel toward the user), -1 = previous.
///
/// Slint's `scroll-event` delta-y (winit `LineDelta` ×60, or logical pixels) is POSITIVE when
/// the wheel turns away from the user, so the sign is inverted here.
#[derive(Debug, Default)]
pub struct WheelCycler {
    acc: f32,
}

/// A single event at least this big is one wheel notch.
pub const WHEEL_NOTCH: f32 = 20.0;
/// Smaller (touchpad) deltas accumulate to this before one step fires.
pub const WHEEL_ACC_STEP: f32 = 50.0;

impl WheelCycler {
    pub fn feed(&mut self, delta_y: f32) -> i32 {
        if delta_y == 0.0 || !delta_y.is_finite() {
            return 0;
        }
        let step = if delta_y > 0.0 { -1 } else { 1 };
        if delta_y.abs() >= WHEEL_NOTCH {
            self.acc = 0.0;
            return step;
        }
        if self.acc != 0.0 && (self.acc > 0.0) != (delta_y > 0.0) {
            self.acc = 0.0;
        }
        self.acc += delta_y;
        if self.acc.abs() >= WHEEL_ACC_STEP {
            self.acc = 0.0;
            step
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TabDock::*;

    const W: f32 = 1000.0;
    const H: f32 = 700.0;
    const ALL: [TabDock; 3] = [Top, Right, Left];

    #[test]
    fn i32_and_persist_mapping() {
        for d in ALL {
            assert_eq!(TabDock::from_i32(d.to_i32()), Some(d));
            assert_eq!(TabDock::from_persisted(d.persisted().as_deref()), d);
        }
        assert_eq!((Top.to_i32(), Right.to_i32(), Left.to_i32()), (0, 1, 2));
        assert_eq!(TabDock::from_i32(7), None);
        assert_eq!(Top.persisted(), None);
        assert_eq!(Right.persisted().as_deref(), Some("right"));
        assert_eq!(Left.persisted().as_deref(), Some("left"));
        assert_eq!(TabDock::from_persisted(Some("bottom")), Top);
        assert_eq!(TabDock::from_persisted(None), Top);
    }

    #[test]
    fn dock_choices_exclude_current() {
        for d in ALL {
            let c = dock_choices(d);
            assert_eq!(c.len(), 2);
            assert!(c.iter().all(|&(_, x)| x != d));
        }
        assert_eq!(dock_choices(Top)[0].0, "Tab bar: dock right");
    }

    #[test]
    fn cycle_order() {
        assert_eq!(Top.cycle(), Right);
        assert_eq!(Right.cycle(), Left);
        assert_eq!(Left.cycle(), Top);
    }

    #[test]
    fn target_zone_edges_from_each_side() {
        for cur in ALL {
            let t = |x, y| dock_target(cur, (x, y), (W, H));
            let want = |d: TabDock| (d != cur).then_some(d);
            // top zone: y < 32
            assert_eq!(t(500.0, 0.0), want(Top));
            assert_eq!(t(500.0, 31.99), want(Top));
            assert_eq!(t(500.0, 32.0), None);
            // top wins over the side zones in the bar
            assert_eq!(t(0.0, 10.0), want(Top));
            assert_eq!(t(W, 10.0), want(Top));
            // left zone: y >= 32, x <= 160
            assert_eq!(t(0.0, 32.0), want(Left));
            assert_eq!(t(160.0, 400.0), want(Left));
            assert_eq!(t(160.01, 400.0), None);
            // right zone: x >= W - 160
            assert_eq!(t(840.0, 32.0), want(Right));
            assert_eq!(t(839.99, 400.0), None);
            assert_eq!(t(W, H), want(Right));
        }
    }

    #[test]
    fn target_is_never_current_or_ambiguous_over_a_grid() {
        for &w in &[480.0_f32, 481.0, 800.0, 1920.0] {
            for cur in ALL {
                let mut y = -5.0;
                while y < 800.0 {
                    let mut x = -5.0;
                    while x < w + 5.0 {
                        let tgt = dock_target(cur, (x, y), (w, H));
                        assert_ne!(tgt, Some(cur));
                        // Zones disjoint: if a target exists, every other `current`
                        // that differs from it agrees on the same target.
                        if let Some(t) = tgt {
                            for other in ALL {
                                if other != t {
                                    assert_eq!(dock_target(other, (x, y), (w, H)), Some(t));
                                }
                            }
                        }
                        x += 7.0;
                    }
                    y += 5.0;
                }
            }
        }
    }

    #[test]
    fn drag_below_threshold_has_no_target() {
        let mut d = DockDrag::begin((50.0, 10.0));
        assert!(!d.moved((52.0, 12.0), Top, (W, H)));
        assert!(!d.active);
        assert_eq!(d.target, None);
        assert_eq!(d.end(), None);
    }

    #[test]
    fn drag_begin_move_commit() {
        let mut d = DockDrag::begin((50.0, 10.0));
        assert!(d.moved((900.0, 300.0), Top, (W, H)));
        assert!(d.active);
        assert_eq!(d.target, Some(Right));
        assert!(!d.moved((905.0, 310.0), Top, (W, H))); // same target
        assert_eq!(d.end(), Some(Right));
    }

    #[test]
    fn drag_moved_back_out_cancels() {
        let mut d = DockDrag::begin((50.0, 10.0));
        d.moved((900.0, 300.0), Top, (W, H));
        assert!(d.moved((500.0, 300.0), Top, (W, H)));
        assert_eq!(d.target, None);
        assert_eq!(d.end(), None);
    }

    #[test]
    fn strip_hit_uses_the_right_axis() {
        let g = [(10.0, 100.0), (110.0, 100.0), (210.0, 100.0)];
        // top → x
        assert_eq!(strip_hit(Top, &g, (120.0, 999.0)), (1, Some(1)));
        assert_eq!(strip_hit(Top, &g, (5.0, 0.0)), (0, None));
        assert_eq!(strip_hit(Top, &g, (500.0, 0.0)), (3, None));
        for d in [Right, Left] {
            // docked → y
            assert_eq!(strip_hit(d, &g, (999.0, 120.0)), (1, Some(1)));
            assert_eq!(strip_hit(d, &g, (0.0, 5.0)), (0, None));
            assert_eq!(strip_hit(d, &g, (0.0, 500.0)), (3, None));
            // past centre of chip 0 but still over it → slot 1
            assert_eq!(strip_hit(d, &g, (0.0, 70.0)), (1, Some(0)));
        }
        assert_eq!(strip_hit(Top, &[], (1.0, 1.0)), (0, None));
    }

    #[test]
    fn over_strip_per_dock() {
        assert!(over_strip(Top, (500.0, 31.0), (0.0, 0.0)));
        assert!(!over_strip(Top, (500.0, 32.0), (0.0, 0.0)));
        let col = (824.0, 176.0);
        assert!(over_strip(Right, (824.0, 32.0), col));
        assert!(over_strip(Right, (999.9, 400.0), col));
        assert!(!over_strip(Right, (823.9, 400.0), col));
        assert!(!over_strip(Right, (1000.0, 400.0), col));
        assert!(!over_strip(Right, (900.0, 31.0), col)); // top bar is not the column
        let col = (0.0, 176.0);
        assert!(over_strip(Left, (0.0, 40.0), col));
        assert!(!over_strip(Left, (176.0, 40.0), col));
    }

    #[test]
    fn area_origin_per_dock() {
        assert_eq!(area_origin_x(Top, (0.0, 176.0)), 0.0);
        assert_eq!(area_origin_x(Right, (824.0, 176.0)), 0.0);
        assert_eq!(area_origin_x(Left, (0.0, 176.0)), 176.0);
    }

    #[test]
    fn wheel_sign_toward_user_is_next() {
        let mut w = WheelCycler::default();
        assert_eq!(w.feed(-60.0), 1); // toward the user
        assert_eq!(w.feed(60.0), -1); // away from the user
        assert_eq!(w.feed(0.0), 0);
    }

    #[test]
    fn wheel_notch_threshold() {
        let mut w = WheelCycler::default();
        assert_eq!(w.feed(-20.0), 1);
        assert_eq!(w.feed(-19.0), 0);
    }

    #[test]
    fn wheel_accumulates_small_deltas_and_resets_on_sign_change() {
        let mut w = WheelCycler::default();
        assert_eq!(w.feed(-10.0), 0);
        assert_eq!(w.feed(-20.0 + 1.0), 0); // 29
        assert_eq!(w.feed(-15.0), 0); // 44
        assert_eq!(w.feed(-10.0), 1); // 54 ≥ 50
        assert_eq!(w.feed(-10.0), 0); // restarted
        assert_eq!(w.feed(10.0), 0); // sign change resets: 10, not -10+10
        assert_eq!(w.feed(10.0), 0);
        assert_eq!(w.feed(10.0), 0);
        assert_eq!(w.feed(10.0), 0);
        assert_eq!(w.feed(10.0), -1);
    }
}
