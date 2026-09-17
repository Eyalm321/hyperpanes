//! Track L1: link base: relative paths resolve against the project roots.
//! Owned by the L1 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! Also the gesture half of the OSC 8 fix. Everything below the widget had unit tests —
//! `grid::hyperlink_at` finds the declared span, `pane::activate_link` turns a hit into
//! `OpenUrl` — but whether a plain click ever *reaches* Rust is decided in `widget.slint`,
//! in the branch that runs while the program owns the mouse. A wrong decision there leaves
//! the whole chain correct and the link still dead under the finger, and no compiler sees
//! it. So it is proven here, against the real component tree.
#![allow(unused_imports)]

use super::*;

/// One terminal pane with a live link hover already published, the way `pane_link_moved`
/// publishes one after the hit-test: the underline is lit at `link-*`, and `grabs` says
/// whether the program inside is holding the mouse (DECSET 1000/1002/1003).
///
/// `declared` is the whole question: true means the program wrapped those cells in an
/// OSC 8 escape, false means the terminal sniffed a URL out of rendered text.
fn install_link_pane(w: &crate::AppWindow, declared: bool, grabs: bool) {
    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            title: "pane 0".into(),
            x: 8.0,
            y: 40.0,
            w: 400.0,
            h: 300.0,
            visible: true,
            focused: true,
            app_grabs_mouse: grabs,
            link_visible: true,
            link_declared: declared,
            link_x: 0.0,
            link_y: 100.0,
            link_w: 200.0,
            link_tip: "https://claude.ai/oauth".into(),
            ..Default::default()
        }]))
        .into(),
    );
    settle();
}

/// Press and release the left button on the pane's body, well clear of the 26px header.
/// Deliberately not `click()`: that takes an `ElementHandle`, and the terminal surface is
/// one custom-rendered rectangle with no accessible child to aim at.
fn click_body(w: &crate::AppWindow) {
    let at = LogicalPosition::new(8.0 + 200.0, 40.0 + 150.0);
    let win = w.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: at });
    win.dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Left,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Left,
    });
}

/// The reported bug, at the layer it was felt: Claude Code's OAuth screen grabs the mouse
/// AND declares its login URL with OSC 8, and a plain click on it did nothing at all. A
/// declared link is not the terminal's guess — the program asked for that click — so it
/// must open without a modifier even while the program owns the mouse.
#[test]
fn a_plain_click_on_a_declared_link_opens_it_while_the_app_holds_the_mouse() {
    ui(|| {
        let w = window();
        install_link_pane(&w, true, true);

        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let reported = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let opened = opened.clone();
            w.on_pane_link_activated(move |i, _x, _y, ctrl| opened.borrow_mut().push((i, ctrl)));
            let reported = reported.clone();
            // Only the button halves are counted. A bare `move` (kind 1) is reported from here
            // unconditionally and filtered in Rust against the DECSET mode — 1003 wants motion,
            // 1002 only wants it with a button down — so counting moves here would be asserting
            // on the wrong layer's business.
            w.on_pane_pointer_report(move |_i, k, _b, _x, _y| {
                if k != 1 {
                    reported.set(reported.get() + 1);
                }
            });
        }

        click_body(&w);

        assert_eq!(
            opened.borrow().as_slice(),
            &[(0, false)],
            "a plain click on a declared link must reach pane-link-activated(0, .., ctrl=false)"
        );
        assert_eq!(
            reported.get(),
            0,
            "both button halves of a link gesture are withheld from the app — reporting only \
             the press would strand a mouse-aware TUI waiting for a release that never comes"
        );
    });
}

/// The other half of the bargain, which the fix must not have broken. A SNIFFED link is
/// the terminal guessing that some rendered text looks like a URL, and a guess must never
/// cost a mouse-aware program a click it was expecting: the click goes to the app, and
/// Ctrl/Cmd (or the Shift escape hatch) is what buys the link.
#[test]
fn a_plain_click_on_a_sniffed_link_still_belongs_to_the_app() {
    ui(|| {
        let w = window();
        install_link_pane(&w, false, true);

        let opened = std::rc::Rc::new(std::cell::Cell::new(0));
        let reported = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let opened = opened.clone();
            w.on_pane_link_activated(move |_i, _x, _y, _ctrl| opened.set(opened.get() + 1));
            let reported = reported.clone();
            // Moves (kind 1) are dropped for the same reason as above: what is at stake here is
            // that the app keeps the BUTTON, and a move report proves nothing either way.
            w.on_pane_pointer_report(move |i, k, b, _x, _y| {
                if k != 1 {
                    reported.borrow_mut().push((i, k, b));
                }
            });
        }

        click_body(&w);

        assert_eq!(
            opened.get(),
            0,
            "an undeclared link must not steal the app's plain click"
        );
        assert_eq!(
            reported.borrow().as_slice(),
            &[(0, 0, 0), (0, 2, 0)],
            "the app must see the press (kind 0) and the release (kind 2) of the left button"
        );
    });
}

/// With no program holding the mouse the pane is plain selectable text, and a left release
/// always asks Rust about the spot — declared or sniffed, that decision is `link_at`'s to
/// make, not the gesture's. Guards against the fix leaking the grab branch's caution into
/// the ordinary shell case, where it would make every path in `ls` output unclickable.
#[test]
fn without_a_grab_a_click_always_asks_rust_about_the_spot() {
    ui(|| {
        let w = window();
        install_link_pane(&w, false, false);

        let opened = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let opened = opened.clone();
            w.on_pane_link_activated(move |_i, _x, _y, _ctrl| opened.set(opened.get() + 1));
        }

        click_body(&w);

        assert_eq!(opened.get(), 1, "a plain shell's click reaches link-activated");
    });
}
