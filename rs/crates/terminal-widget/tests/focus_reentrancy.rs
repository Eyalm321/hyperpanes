//! AccessKit focus re-entrancy regression tests for the `TerminalPane` Slint component.
//!
//! Pins goal `trcrash`: with an AT-SPI client listening (AccessKit active — GNOME, Orca,
//! automation), closing the New goal box panicked the app with `RefCell
//! already borrowed` in `WinitWindowAdapter::handle_focus_change`.
//!
//! The chain, at the pinned Slint rev: removing the overlay makes AccessKit schedule a
//! `reload_tree`. That timer callback borrows the window's `RefCell<AccessKitAdapter>` and calls
//! `NodeCollection::build_new_tree`, which calls `WindowInner::ensure_tree_instantiated`, which
//! runs every PENDING change handler (and the `init` of every newly created conditional) while
//! the borrow is still held. Closing the overlay had also bumped this pane's `refocus-tick`, so
//! `changed refocus-tick => { fs.focus(); }` ran right there. `focus()` → `set_focus_item` →
//! `handle_focus_change` → `accesskit_adapter.borrow_mut()` → panic.
//!
//! Fix: the widget's focus handlers no longer call `focus()` themselves. They arm a 0 ms
//! single-fire `Timer`, and the timer moves focus on the next event-loop turn, outside any
//! AccessKit borrow.
//!
//! The headless testing backend has no AccessKit, so these tests use their own `WindowAdapter`
//! whose `WindowAdapterInternal::handle_focus_change` does what the winit adapter does
//! (`borrow_mut()` a `RefCell`), plus an `accessibility_tree_build()` that holds that borrow
//! across `ensure_tree_instantiated`, as `build_new_tree` does. On the unfixed widget each
//! handler panics inside that build (red). On the fix the build moves no focus, and after one
//! timer turn the focus is where the old synchronous handler put it (green; behavior kept).

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use hyperpanes_terminal_widget::ui::{DemoWindow, PaneVisual};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::window::{WindowAdapterInternal, WindowInner};
use slint::platform::software_renderer::SoftwareRenderer;
use slint::platform::{Platform, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, Model, PhysicalSize, VecModel};

/// A window adapter that behaves like `WinitWindowAdapter` with AccessKit active, as far as
/// focus changes go.
struct A11yTrapWindow {
    window: slint::Window,
    renderer: SoftwareRenderer,
    size: Cell<PhysicalSize>,
    /// Stands in for the winit adapter's `RefCell<AccessKitAdapter>`.
    accesskit: RefCell<()>,
    focus_changes: Cell<u32>,
}

impl A11yTrapWindow {
    fn new() -> Rc<Self> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Self>| A11yTrapWindow {
            window: slint::Window::new(weak.clone() as std::rc::Weak<dyn WindowAdapter>),
            renderer: SoftwareRenderer::new(),
            size: Cell::new(PhysicalSize::new(800, 600)),
            accesskit: RefCell::new(()),
            focus_changes: Cell::new(0),
        })
    }

    /// What `AccessKitAdapter::reload_tree`'s timer does: hold the adapter borrow across
    /// `build_new_tree` → `ensure_tree_instantiated`. Returns whether it panicked.
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
        self.size.get()
    }
    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
    fn internal(&self, _: i_slint_core::InternalToken) -> Option<&dyn WindowAdapterInternal> {
        Some(self)
    }
}

impl WindowAdapterInternal for A11yTrapWindow {
    // `WinitWindowAdapter::handle_focus_change` at the pinned rev:
    //     accesskit_adapter_cell.borrow_mut().handle_focus_item_change();
    fn handle_focus_change(&self, _old: Option<ItemRc>, _new: Option<ItemRc>) {
        let _adapter = self.accesskit.borrow_mut();
        self.focus_changes.set(self.focus_changes.get() + 1);
    }
}

struct TrapPlatform {
    window: Rc<A11yTrapWindow>,
}

impl Platform for TrapPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

/// Keys / search edits the panes reported, as `(pane, text)`.
type Log = Rc<RefCell<Vec<(i32, String)>>>;

struct Harness {
    trap: Rc<A11yTrapWindow>,
    win: DemoWindow,
    panes: Rc<VecModel<PaneVisual>>,
    keys: Log,
    edits: Log,
}

impl Harness {
    fn new(n: usize) -> Self {
        let trap = A11yTrapWindow::new();
        slint::platform::set_platform(Box::new(TrapPlatform {
            window: trap.clone(),
        }))
        .expect("one platform per test thread");

        let win = DemoWindow::new().unwrap();
        let panes = Rc::new(VecModel::from(vec![PaneVisual::default(); n]));
        win.set_panes(panes.clone().into());
        let keys: Log = Rc::default();
        win.on_key({
            let k = keys.clone();
            move |idx, m| k.borrow_mut().push((idx, m.text.to_string()))
        });
        let edits: Log = Rc::default();
        win.on_search_edited({
            let e = edits.clone();
            move |idx, q| e.borrow_mut().push((idx, q.to_string()))
        });
        win.show().unwrap();
        win.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(800.0, 600.0),
        });
        // Settle the initial instantiation OUTSIDE any accessibility borrow (nothing here
        // focuses on creation; this just makes the later builds see only our change).
        WindowInner::from_pub(win.window()).ensure_tree_instantiated();
        slint::platform::update_timers_and_animations();
        Harness {
            trap,
            win,
            panes,
            keys,
            edits,
        }
    }

    fn edit_pane(&self, idx: usize, f: impl FnOnce(&mut PaneVisual)) {
        let mut row = self.panes.row_data(idx).unwrap();
        f(&mut row);
        self.panes.set_row_data(idx, row);
    }

    /// Run the AccessKit-shaped tree build right after a property change (the crash
    /// ordering) and assert it neither panicked nor moved focus synchronously.
    fn a11y_build_moves_no_focus(&self, what: &str) {
        let before = self.trap.focus_changes.get();
        if let Err(msg) = self.trap.accessibility_tree_build() {
            panic!(
                "{what}: a focus change ran INSIDE the accessibility tree build and re-borrowed \
                 the AccessKit adapter (the trcrash panic: `{msg}`). Focus moves from a change \
                 handler / init must be deferred to a Timer, never called synchronously."
            );
        }
        assert_eq!(
            self.trap.focus_changes.get(),
            before,
            "{what}: the change handler moved focus synchronously during the tree build"
        );
    }

    /// One event-loop turn: fires the deferred-focus timers (outside any borrow).
    fn next_turn(&self) {
        slint::platform::update_timers_and_animations();
        slint::platform::update_timers_and_animations();
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

#[test]
fn refocus_tick_defers_focus_out_of_the_accessibility_build() {
    let h = Harness::new(2);

    // Nothing is focused yet, so typed keys reach no pane.
    h.type_text("a");
    assert!(h.keys.borrow().is_empty(), "precondition: no pane focused");

    // Closing a keyboard overlay bumps the active pane's refocus tick (state.rs
    // close_overlay_now → refocus_active_pane_scope). AccessKit's reload then runs the handler.
    h.edit_pane(1, |p| p.refocus_tick += 1);
    h.a11y_build_moves_no_focus("refocus-tick");

    // Behavior preserved: on the next turn the terminal scope of pane 1 owns the keyboard.
    h.next_turn();
    assert_eq!(
        h.trap.focus_changes.get(),
        1,
        "the deferred focus must land exactly once"
    );
    h.type_text("b");
    assert_eq!(*h.keys.borrow(), vec![(1, "b".to_string())]);

    h.win.hide().unwrap();
}

#[test]
fn search_focus_handlers_defer_focus_out_of_the_accessibility_build() {
    let h = Harness::new(2);

    // Opening the search box focuses its query input (`changed search-open`).
    h.edit_pane(0, |p| p.search_open = true);
    h.a11y_build_moves_no_focus("search-open (open)");
    h.next_turn();
    h.type_text("x");
    assert_eq!(*h.edits.borrow(), vec![(0, "x".to_string())]);

    // The keyboard moves to pane 1's terminal (refocus tick; its search is closed).
    h.edit_pane(1, |p| p.refocus_tick += 1);
    h.a11y_build_moves_no_focus("refocus-tick");
    h.next_turn();
    h.type_text("k");
    assert_eq!(*h.keys.borrow(), vec![(1, "k".to_string())]);

    // Re-invoking search on the already-open box re-focuses its query (`search-focus-tick`).
    h.edit_pane(0, |p| p.search_focus_tick += 1);
    h.a11y_build_moves_no_focus("search-focus-tick");
    h.next_turn();
    h.type_text("y");
    assert_eq!(
        *h.edits.borrow(),
        vec![(0, "x".to_string()), (0, "xy".to_string())]
    );

    // Closing the box hands the keyboard back to pane 0's terminal (`changed search-open`).
    h.edit_pane(0, |p| p.search_open = false);
    h.a11y_build_moves_no_focus("search-open (close)");
    h.next_turn();
    h.type_text("z");
    assert_eq!(
        *h.keys.borrow(),
        vec![(1, "k".to_string()), (0, "z".to_string())]
    );

    h.win.hide().unwrap();
}
