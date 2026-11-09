//! AccessKit focus re-entrancy regression tests on the real `AppWindow` tree (goal trcrash).
//!
//! The crash: with an AT-SPI client listening (AccessKit active), cancelling the New goal box
//! panicked with `RefCell already borrowed` in `WinitWindowAdapter::handle_focus_change`.
//! `State::close_overlay_now` drops the overlay AND bumps the active pane's `refocus-seq`.
//! Removing the overlay makes AccessKit schedule `reload_tree`. Its timer borrows the window's
//! `RefCell<AccessKitAdapter>` and calls `build_new_tree` → `ensure_tree_instantiated`, which
//! runs pending change handlers and the `init` of newly created conditionals INSIDE the borrow.
//! The pane's `changed refocus-tick => { fs.focus(); }` ran there; `focus()` →
//! `handle_focus_change` → `borrow_mut()` → panic. Any `init => { x.focus(); }` in a
//! conditionally created dialog/editor has the same shape.
//!
//! Fix: no `focus()` runs synchronously in a change handler or `init`; each arms a 0 ms
//! single-fire Slint `Timer` that moves focus on the next event-loop turn, outside any borrow.
//! `tests/slint_focus_guard.rs` keeps new handlers from regressing at the source level.
//!
//! The adapter below copies the focus-relevant part of the winit adapter with AccessKit on:
//! `handle_focus_change` does `borrow_mut()`, and `accessibility_tree_build()` holds that
//! borrow across `ensure_tree_instantiated`, as `build_new_tree` does. Unfixed UI: each case
//! panics inside the build (red). Fixed UI: the build moves no focus, and after one timer
//! turn focus is where the old handler put it (green).

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use i_slint_core::item_tree::ItemRc;
use i_slint_core::window::{WindowAdapterInternal, WindowInner};
use slint::platform::software_renderer::SoftwareRenderer;
use slint::platform::{Platform, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, Model, PhysicalSize, VecModel};

use crate::{AppWindow, PaneItem};

/// Overlay kinds (`app.slint` `overlay-kind`): 3 new-pane · 4 add-project · 5 new-goal.
const OVERLAY_NEW_PANE: i32 = 3;
const OVERLAY_ADD_PROJECT: i32 = 4;
const OVERLAY_NEW_GOAL: i32 = 5;

struct A11yTrapWindow {
    window: slint::Window,
    renderer: SoftwareRenderer,
    /// Stands in for the winit adapter's `RefCell<AccessKitAdapter>`.
    accesskit: RefCell<()>,
    focus_changes: Cell<u32>,
}

impl A11yTrapWindow {
    fn new() -> Rc<Self> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Self>| A11yTrapWindow {
            window: slint::Window::new(weak.clone() as std::rc::Weak<dyn WindowAdapter>),
            renderer: SoftwareRenderer::new(),
            accesskit: RefCell::new(()),
            focus_changes: Cell::new(0),
        })
    }

    /// `AccessKitAdapter::reload_tree`'s timer body: hold the adapter borrow across
    /// `build_new_tree` → `ensure_tree_instantiated`.
    fn accessibility_tree_build(&self) -> Result<(), String> {
        catch_unwind(AssertUnwindSafe(|| {
            let _adapter = self.accesskit.borrow_mut();
            WindowInner::from_pub(&self.window).ensure_tree_instantiated();
        }))
        .map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        })
    }
}

impl WindowAdapter for A11yTrapWindow {
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

impl WindowAdapterInternal for A11yTrapWindow {
    // `WinitWindowAdapter::handle_focus_change` at the pinned Slint rev.
    fn handle_focus_change(&self, _old: Option<ItemRc>, _new: Option<ItemRc>) {
        let _adapter = self.accesskit.borrow_mut();
        self.focus_changes.set(self.focus_changes.get() + 1);
    }
}

struct TrapPlatform(Rc<A11yTrapWindow>);

impl Platform for TrapPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

struct Harness {
    trap: Rc<A11yTrapWindow>,
    win: AppWindow,
    panes: Rc<VecModel<PaneItem>>,
    keys: Rc<RefCell<Vec<(i32, String)>>>,
    goal_text: Rc<RefCell<Vec<String>>>,
}

impl Harness {
    fn new() -> Self {
        let trap = A11yTrapWindow::new();
        slint::platform::set_platform(Box::new(TrapPlatform(trap.clone())))
            .expect("one platform per test thread");
        let win = AppWindow::new().unwrap();
        let pane = PaneItem {
            x: 0.0,
            y: 0.0,
            w: 900.0,
            h: 700.0,
            visible: true,
            focused: true,
            ..Default::default()
        };
        let panes = Rc::new(VecModel::from(vec![pane]));
        win.set_panes(panes.clone().into());
        let keys: Rc<RefCell<Vec<(i32, String)>>> = Rc::default();
        win.on_key({
            let k = keys.clone();
            move |idx, m| k.borrow_mut().push((idx, m.text.to_string()))
        });
        let goal_text: Rc<RefCell<Vec<String>>> = Rc::default();
        win.on_goal_text_changed({
            let g = goal_text.clone();
            move |t| g.borrow_mut().push(t.to_string())
        });
        win.show().unwrap();
        win.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(1200.0, 800.0),
        });
        let h = Harness {
            trap,
            win,
            panes,
            keys,
            goal_text,
        };
        h.settle();
        h
    }

    /// Instantiate + run handlers OUTSIDE any accessibility borrow, then fire timers.
    fn settle(&self) {
        WindowInner::from_pub(self.win.window()).ensure_tree_instantiated();
        self.next_turn();
    }

    fn next_turn(&self) {
        slint::platform::update_timers_and_animations();
        slint::platform::update_timers_and_animations();
    }

    fn bump_refocus(&self) {
        let mut p = self.panes.row_data(0).unwrap();
        p.refocus_seq += 1;
        self.panes.set_row_data(0, p);
    }

    /// The AccessKit-shaped build right after a UI change (the crash ordering): must neither
    /// panic nor move focus synchronously.
    fn a11y_build_moves_no_focus(&self, what: &str) {
        let before = self.trap.focus_changes.get();
        if let Err(msg) = self.trap.accessibility_tree_build() {
            panic!(
                "{what}: a focus change ran INSIDE the accessibility tree build and re-borrowed \
                 the AccessKit adapter (the trcrash panic: `{msg}`). Defer focus moves from \
                 change handlers / init to a Timer."
            );
        }
        assert_eq!(
            self.trap.focus_changes.get(),
            before,
            "{what}: focus moved synchronously during the accessibility tree build"
        );
    }

    fn type_text(&self, text: &str) {
        for ch in text.chars() {
            let t: slint::SharedString = ch.to_string().into();
            self.win
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: t.clone() });
            self.win
                .window()
                .dispatch_event(WindowEvent::KeyReleased { text: t });
        }
    }
}

/// The user's crash, literally: New goal box open, Cancel → `close_overlay_now` drops the
/// overlay and bumps the pane's refocus tick; AccessKit rebuilds the tree.
#[test]
fn cancelling_new_goal_box_refocuses_terminal_outside_accessibility_build() {
    let h = Harness::new();

    h.win.set_overlay_kind(OVERLAY_NEW_GOAL);
    h.settle();
    h.win.set_goal_focus_tick(h.win.get_goal_focus_tick() + 1);
    h.settle();
    h.type_text("g");
    assert_eq!(
        *h.goal_text.borrow(),
        vec!["g".to_string()],
        "precondition: the goal box owns the keyboard"
    );

    // Cancel.
    h.win.set_overlay_kind(0);
    h.bump_refocus();
    h.a11y_build_moves_no_focus("cancel New goal (refocus-tick)");

    // Behavior preserved: keyboard focus is back on the terminal on the next turn.
    h.next_turn();
    h.type_text("t");
    assert_eq!(*h.keys.borrow(), vec![(0, "t".to_string())]);

    h.win.hide().unwrap();
}

/// Opening the New goal box re-focuses its TextInput via `focus-tick`.
#[test]
fn new_goal_focus_tick_focuses_outside_accessibility_build() {
    let h = Harness::new();

    h.win.set_overlay_kind(OVERLAY_NEW_GOAL);
    h.settle();
    h.win.set_goal_focus_tick(h.win.get_goal_focus_tick() + 1);
    h.a11y_build_moves_no_focus("New goal focus-tick");

    h.next_turn();
    h.type_text("x");
    assert_eq!(*h.goal_text.borrow(), vec!["x".to_string()]);

    h.win.hide().unwrap();
}

/// Dialogs that grab focus on creation (`init`) are instantiated by the accessibility build
/// itself when it runs before the next render.
#[test]
fn dialog_mount_focus_happens_outside_accessibility_build() {
    let h = Harness::new();
    for (kind, what) in [
        (OVERLAY_ADD_PROJECT, "Add project dialog mount"),
        (OVERLAY_NEW_PANE, "New pane dialog mount"),
    ] {
        let before = h.trap.focus_changes.get();
        h.win.set_overlay_kind(kind);
        h.a11y_build_moves_no_focus(what);
        h.next_turn();
        assert!(
            h.trap.focus_changes.get() > before,
            "{what}: the deferred mount focus must still land on the next turn"
        );
        h.win.set_overlay_kind(0);
        h.settle();
    }
    h.win.hide().unwrap();
}
