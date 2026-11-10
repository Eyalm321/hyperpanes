//! Tab-bar dock UI tests (t25, #85): the grip's hit box, cursor (`grab` on hover, `grabbing`
//! while pressed/dragging, default elsewhere), drag callbacks, the tab wheel, and the grip's
//! accessibility role/label, driven through a real `AppWindow` on a software-renderer adapter.
//!
//! Layout constants the geometry relies on (window 1200x800 @ scale 1, rail 44px):
//! - top bar 32px; hamburger starts at x = 10 (pad) and is 30px wide, spacing 8 → the top grip
//!   (14x24, vertically centred in the bar) spans x 48..62, y 4..28 → centre (55, 16);
//! - docked column is 176px; its 32px header centres the grip at x + 8 (14x24) below the bar →
//!   left column centre (15, 48); right column x = 1200 - 44 - 176 = 980 → centre (995, 48);
//! - the pane-area centre (600, 400) is outside every column and carries no cursor.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_core::accessibility::AccessibleStringProperty;
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::{AccessibleRole, MouseCursor};
use i_slint_core::window::{WindowAdapterInternal, WindowInner};
use slint::platform::software_renderer::SoftwareRenderer;
use slint::platform::{Platform, PointerEventButton, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, PhysicalSize, SharedString, VecModel};

use crate::{AppWindow, TabItem};

struct CursorWindow {
    window: slint::Window,
    renderer: SoftwareRenderer,
    cursors: RefCell<Vec<MouseCursor>>,
}

impl CursorWindow {
    fn new() -> Rc<Self> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Self>| CursorWindow {
            window: slint::Window::new(weak.clone() as std::rc::Weak<dyn WindowAdapter>),
            renderer: SoftwareRenderer::new(),
            cursors: RefCell::new(Vec::new()),
        })
    }
    fn last_cursor(&self) -> MouseCursor {
        self.cursors
            .borrow()
            .last()
            .copied()
            .unwrap_or(MouseCursor::Default)
    }
}

impl WindowAdapter for CursorWindow {
    fn window(&self) -> &slint::Window {
        &self.window
    }
    fn size(&self) -> PhysicalSize {
        PhysicalSize::new(1200, 800)
    }
    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
    fn internal(&self, _: i_slint_core::InternalToken) -> Option<&dyn WindowAdapterInternal> {
        Some(self)
    }
}

impl WindowAdapterInternal for CursorWindow {
    fn set_mouse_cursor(&self, cursor: MouseCursor) {
        self.cursors.borrow_mut().push(cursor);
    }
}

struct Plat(Rc<CursorWindow>);

impl Platform for Plat {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct Log {
    begin: Vec<(f32, f32)>,
    moves: Vec<(f32, f32)>,
    end: u32,
    wheel: Vec<(f32, bool)>,
}

struct H {
    cw: Rc<CursorWindow>,
    win: AppWindow,
    log: Rc<RefCell<Log>>,
}

impl H {
    fn new() -> Self {
        let cw = CursorWindow::new();
        slint::platform::set_platform(Box::new(Plat(cw.clone())))
            .expect("one platform per test thread");
        let win = AppWindow::new().unwrap();
        let tabs: Vec<TabItem> = (0..3)
            .map(|i| TabItem {
                title: format!("tab {i}").into(),
                active: i == 0,
                has_color: false,
                color: Default::default(),
            })
            .collect();
        win.set_tabs(ModelRc::from(Rc::new(VecModel::from(tabs))));
        win.set_show_sidebar(true);
        let log: Rc<RefCell<Log>> = Rc::default();
        win.on_dock_drag_begin({
            let l = log.clone();
            move |x, y| l.borrow_mut().begin.push((x, y))
        });
        win.on_dock_drag_move({
            let l = log.clone();
            move |x, y| l.borrow_mut().moves.push((x, y))
        });
        win.on_dock_drag_end({
            let l = log.clone();
            move || l.borrow_mut().end += 1
        });
        win.on_tab_wheel({
            let l = log.clone();
            move |d, s| l.borrow_mut().wheel.push((d, s))
        });
        win.show().unwrap();
        win.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(1200.0, 800.0),
        });
        let h = H { cw, win, log };
        h.settle();
        h
    }

    fn settle(&self) {
        WindowInner::from_pub(self.win.window()).ensure_tree_instantiated();
        slint::platform::update_timers_and_animations();
    }

    fn ev(&self, e: WindowEvent) {
        self.win.window().dispatch_event(e);
    }
    fn mv(&self, x: f32, y: f32) {
        self.ev(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        });
    }
}

/// Grip centre for each dock side (see module docs).
fn grip_centre(dock: i32) -> (f32, f32) {
    match dock {
        0 => (55.0, 16.0),
        1 => (995.0, 48.0),
        _ => (15.0, 48.0),
    }
}

#[test]
fn grip_cursor_and_drag_callbacks_for_every_dock_side() {
    let h = H::new();
    for dock in 0..=2 {
        h.win.set_tab_dock(dock);
        h.settle();
        *h.log.borrow_mut() = Log::default();
        let (gx, gy) = grip_centre(dock);

        // a) hover → grab
        h.mv(gx, gy);
        assert_eq!(h.cw.last_cursor(), MouseCursor::Grab, "dock {dock}: hover");

        // b) press + drag → grabbing, begin + move with window-logical coords
        h.ev(WindowEvent::PointerPressed {
            position: LogicalPosition::new(gx, gy),
            button: PointerEventButton::Left,
        });
        h.mv(gx + 30.0, gy + 30.0);
        assert_eq!(
            h.cw.last_cursor(),
            MouseCursor::Grabbing,
            "dock {dock}: drag"
        );
        {
            let l = h.log.borrow();
            assert_eq!(l.begin.len(), 1, "dock {dock}: dock-drag-begin");
            assert!((l.begin[0].0 - gx).abs() < 1.5 && (l.begin[0].1 - gy).abs() < 1.5);
            let m = l.moves.last().expect("dock-drag-move fired");
            assert!((m.0 - (gx + 30.0)).abs() < 1.5 && (m.1 - (gy + 30.0)).abs() < 1.5);
        }

        // c) release → end; back over the panes → default
        h.ev(WindowEvent::PointerReleased {
            position: LogicalPosition::new(gx + 30.0, gy + 30.0),
            button: PointerEventButton::Left,
        });
        assert_eq!(h.log.borrow().end, 1, "dock {dock}: dock-drag-end");
        h.mv(600.0, 400.0);
        assert_eq!(
            h.cw.last_cursor(),
            MouseCursor::Default,
            "dock {dock}: panes"
        );
    }
    h.win.hide().unwrap();
}

#[test]
fn wheel_over_top_chip_fires_tab_wheel() {
    let h = H::new();
    h.settle();
    // first chip: after the grip (62) + strip padding 6 → x 68.., 96px min width; y 4..32.
    h.ev(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(100.0, 18.0),
        delta_x: 0.0,
        delta_y: -30.0,
    });
    let l = h.log.borrow();
    assert_eq!(l.wheel.len(), 1, "tab-wheel fired once");
    assert_eq!(l.wheel[0], (-30.0, false));
}

#[test]
fn wheel_over_docked_column_fires_tab_wheel_when_not_overflowing() {
    let h = H::new();
    h.win.set_tab_dock(2);
    h.settle();
    h.ev(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(80.0, 32.0 + 32.0 + 14.0),
        delta_x: 0.0,
        delta_y: -30.0,
    });
    assert_eq!(h.log.borrow().wheel.len(), 1);
}

fn find_role(root: ItemRc, role: AccessibleRole, out: &mut Vec<ItemRc>) {
    if root.accessible_role() == role {
        out.push(root.clone());
    }
    let mut c = root.first_child();
    while let Some(ch) = c {
        find_role(ch.clone(), role, out);
        c = ch.next_sibling();
    }
}

#[test]
fn grip_is_an_accessible_button_with_label() {
    let h = H::new();
    let comp = WindowInner::from_pub(h.win.window()).component();
    let mut buttons = Vec::new();
    find_role(ItemRc::new(comp, 0), AccessibleRole::Button, &mut buttons);
    let labels: Vec<SharedString> = buttons
        .iter()
        .filter_map(|b| b.accessible_string_property(AccessibleStringProperty::Label))
        .collect();
    assert!(
        labels.iter().any(|l| l == "Tab bar handle"),
        "grip button missing; labels = {labels:?}"
    );
}
