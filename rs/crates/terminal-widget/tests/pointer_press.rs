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
//! Fixed by `focus-on-click: false` on the scope (`ta` calls `fs.focus()` on every down).
//!
//! These tests dispatch raw pointer events through the headless testing platform — the
//! same `WindowInner::process_mouse_input` path the live app uses — with NO prior focus,
//! exactly the state the live bug needed.

use std::cell::Cell;
use std::rc::Rc;

use hyperpanes_terminal_widget::ui::TerminalPane;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};

#[test]
fn first_press_reaches_touch_area_without_prior_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let pos = LogicalPosition::new(200.0, 150.0);

    // ---- right button: the #32 right-click-paste path ----
    let pane = TerminalPane::new().unwrap();
    pane.window().set_size(slint::PhysicalSize::new(400, 300));
    let pasted = Rc::new(Cell::new(0u32));
    pane.on_paste_requested({
        let p = pasted.clone();
        move || p.set(p.get() + 1)
    });
    let focused = Rc::new(Cell::new(0u32));
    pane.on_focus_requested({
        let f = focused.clone();
        move || f.set(f.get() + 1)
    });
    pane.show().unwrap();

    pane.window().dispatch_event(WindowEvent::PointerMoved { position: pos });
    pane.window()
        .dispatch_event(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Right });
    pane.window()
        .dispatch_event(WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Right });

    assert_eq!(
        pasted.get(),
        1,
        "the FIRST right-press into a pane with no prior Slint focus must reach the \
         TouchArea and request a paste (the FocusScope must not consume it)"
    );
    assert_eq!(focused.get(), 1, "the same press must fire the frozen focus-requested contract");
    pane.hide().unwrap();

    // ---- left button: the drag-selection anchor (#33 type-over depends on it) ----
    let pane = TerminalPane::new().unwrap();
    pane.window().set_size(slint::PhysicalSize::new(400, 300));
    let begun = Rc::new(Cell::new(0u32));
    pane.on_selection_begin({
        let b = begun.clone();
        move |_x, _y| b.set(b.get() + 1)
    });
    pane.show().unwrap();

    pane.window().dispatch_event(WindowEvent::PointerMoved { position: pos });
    pane.window()
        .dispatch_event(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left });
    pane.window()
        .dispatch_event(WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Left });

    assert_eq!(
        begun.get(),
        1,
        "the FIRST left-press must anchor a selection — a swallowed anchor press makes \
         the drag select nothing (and copy-on-select then re-copies a stale selection)"
    );
    pane.hide().unwrap();
}
