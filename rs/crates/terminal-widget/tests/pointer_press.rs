//! Pointer-dispatch regression tests for the `TerminalPane` Slint component.
//!
//! Pins the wave3z2 root cause: a `FocusScope` with the default `focus-on-click: true`
//! CONSUMES the first mouse-press whenever it doesn't already have focus (Slint's
//! `FocusScope::input_event` grabs focus and returns `EventAccepted`), and the pane's
//! `fs` scope is a later sibling of the pointer `TouchArea`, so it hit-tests first.
//! That ate the button-DOWN of the first click into any not-Slint-focused pane:
//!  * right-click paste (#32) silently no-opped on an unfocused pane, and
//!  * a drag-selection's anchor press was lost, so the drag selected nothing — with
//!    copy-on-select ON the release then re-copied a STALE selection and type-over
//!    (#33) erased against it.
//!
//! Fixed by `focus-on-click: false` on the scope (`ta` calls `fs.focus()` on every down).
//!
//! `TerminalPane` inherits Rectangle (no generated Rust type), so the test drives the
//! `DemoWindow` host — two real `TerminalPane`s in a layout, the same shape as the app's
//! pane view — through the headless testing platform's raw pointer dispatch (the same
//! `WindowInner::process_mouse_input` path the live app uses), with NO prior focus:
//! exactly the state the live bug needed.

use std::cell::RefCell;
use std::rc::Rc;

use hyperpanes_terminal_widget::ui::{DemoWindow, PaneVisual};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model};

#[test]
fn first_press_reaches_touch_area_without_prior_focus() {
    i_slint_backend_testing::init_no_event_loop();

    let win = DemoWindow::new().unwrap();
    let panes = Rc::new(slint::VecModel::from(vec![PaneVisual::default(), PaneVisual::default()]));
    assert_eq!(panes.row_count(), 2);
    win.set_panes(panes.into());
    win.window().set_size(slint::PhysicalSize::new(800, 600));

    let pasted: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    win.on_paste_requested({
        let p = pasted.clone();
        move |idx| p.borrow_mut().push(idx)
    });
    let focused: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    win.on_focus_requested({
        let f = focused.clone();
        move |idx| f.borrow_mut().push(idx)
    });
    let begun: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    win.on_selection_begin({
        let b = begun.clone();
        move |idx, _x, _y| b.borrow_mut().push(idx)
    });
    win.show().unwrap();

    // The demo lays the two panes out side by side (16px padding/spacing): pane 0 spans
    // roughly x 16..392, pane 1 x 408..784 in the 800px window.
    let in_pane0 = LogicalPosition::new(200.0, 300.0);
    let in_pane1 = LogicalPosition::new(600.0, 300.0);

    // ---- right button into pane 1, no pane has Slint focus yet (the #32 paste path) ----
    win.window().dispatch_event(WindowEvent::PointerMoved { position: in_pane1 });
    win.window()
        .dispatch_event(WindowEvent::PointerPressed { position: in_pane1, button: PointerEventButton::Right });
    win.window()
        .dispatch_event(WindowEvent::PointerReleased { position: in_pane1, button: PointerEventButton::Right });

    assert_eq!(
        *pasted.borrow(),
        vec![1],
        "the FIRST right-press into a pane with no prior Slint focus must reach the \
         TouchArea and request a paste (the FocusScope must not consume it)"
    );
    assert_eq!(*focused.borrow(), vec![1], "the same press must fire the frozen focus-requested contract");

    // ---- left button into pane 0, whose scope is NOT the focused one (the #33 drag anchor) ----
    win.window().dispatch_event(WindowEvent::PointerMoved { position: in_pane0 });
    win.window()
        .dispatch_event(WindowEvent::PointerPressed { position: in_pane0, button: PointerEventButton::Left });
    win.window()
        .dispatch_event(WindowEvent::PointerReleased { position: in_pane0, button: PointerEventButton::Left });

    assert_eq!(
        *begun.borrow(),
        vec![0],
        "the FIRST left-press into a not-focused pane must anchor a selection — a \
         swallowed anchor press makes the drag select nothing (and copy-on-select then \
         re-copies a stale selection)"
    );
    assert_eq!(*focused.borrow(), vec![1, 0]);

    win.hide().unwrap();
}
