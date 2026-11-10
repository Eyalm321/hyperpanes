# Tab bar: docking, cycling and the wheel

## Docking

The tab strip starts with a 2-column dot **grip**. Drag it and the whole tab group moves to
another edge of the window:

- **Top** (default): the horizontal strip in the 32 px top bar.
- **Right** / **Left**: a 176 px vertical column. The top bar stays (hamburger, window-drag
  area, window controls) and the column sits beside the panes: the right column between the
  panes and the sidebar rail, the left column at the far left of the window. Only the pane
  area's width changes; nothing moves vertically. Columns are hidden in fullscreen, like the
  top bar.

### Drop zones

Positions are window-logical pixels (`W` = window width). The three zones are disjoint, and a
side's own zone is never a target, so the result is never the current side:

| Zone  | Region                      |
|-------|-----------------------------|
| Top   | `y < 32`                    |
| Left  | `y >= 32` and `x <= 160`    |
| Right | `y >= 32` and `x >= W - 160`|

The side zones need `y >= 32` because the top-docked grip itself sits inside the top bar, so
reaching a side means pulling the pointer down out of the bar. A press becomes a drag after
4 px of movement. Releasing over a zone docks there; releasing anywhere else cancels.

The cursor is `grab` over the grip and `grabbing` while pressed or dragging. Without a mouse,
the grip's accessible default action cycles top, right, left, top. The command palette and the
tab context menu also offer "Tab bar: move to top / dock right / dock left" (the current
position is omitted).

### Persistence

The side is per window and is stored in the session snapshot as `"tabDock": "right"` or
`"left"` (see `docs/hyperpanes-format.md`). Top is the default and the field is omitted. An
unknown value means top. Saving a workspace template never writes it.

Caveat: today only the restored (last) window's snapshot persists, so detached windows are not
restored across restarts and their dock side is not either.

## Cycling

| Action | Default |
|--------|---------|
| Next tab | Ctrl+Tab, Ctrl+PgDn |
| Previous tab | Ctrl+Shift+Tab, Ctrl+PgUp |
| Go to tab 1..8 / last tab (9) | unbound; bind in Preferences > Keybindings |

Cycling wraps around.

### Wheel

- Over the **top** strip, the wheel cycles tabs.
- In a **docked column**: if the tab list overflows, the plain wheel scrolls the list and
  Shift+wheel cycles; if it does not overflow, the wheel cycles.
- Wheel toward the user = next tab. A mouse notch (a single event of 20 or more units) is one
  step; smaller touchpad deltas accumulate to 50 before one step, and the accumulator resets
  when the direction changes.

## Keybinding decision table

Chords the app binds are swallowed, so the focused pane program never sees them. Unbinding one
in Preferences > Keybindings hands it back to the pane.

| Binding | Default | What a pane program loses | Decision | How to override |
|---------|---------|---------------------------|----------|-----------------|
| Ctrl+Tab, Ctrl+Shift+Tab | on | Legacy xterm encoding sends a plain TAB for these, so apps cannot tell them from Tab anyway. Only apps using the kitty keyboard protocol or modifyOtherKeys lose a distinct code. | ON | Unbind or rebind `tab.next` / `tab.prev` |
| Ctrl+PgDn, Ctrl+PgUp | on | Sent as `CSI 6;5~` / `CSI 5;5~`. Terminal vim maps `<C-PageDown>` / `<C-PageUp>` to next/previous tab page by default; emacs binds C-next / C-prior to scroll-left/right; some tmux and mc setups use them too. | ON, documented | Unbind `tab.nextPage` / `tab.prevPage` to hand them back |
| Ctrl+1..9 | off | Legacy encoding sends Ctrl+2 = NUL (emacs set-mark), Ctrl+3..7 = ESC/FS/GS/RS/US and Ctrl+8 = DEL. | OFF by default | Bind `tab.goto1`..`tab.goto9` (9 = last tab) in Preferences > Keybindings |
| Ctrl/Cmd+click on a pane header | on (chrome only) | Nothing: the click lands on the header, not the terminal. Ctrl+click inside terminal content still copies a clicked path. | ON | none (a mouse gesture, not a chord) |
| Ctrl+G (`pane.groupSelection`) | on, **guarded** | Nothing unless 2+ panes are selected. With no selection the key goes to the pane as BEL (0x07), so emacs, nano and less keep it. | ON, guarded | Rebind or unbind `pane.groupSelection` |
| Esc (`pane.clearSelection`) | on, **guarded** | Nothing unless 2+ panes are selected. With no selection Esc reaches the pane unchanged (0x1b). | ON, guarded | Rebind or unbind `pane.clearSelection` |
| Ungroup tab (`tab.ungroup`) | off | Nothing: unbound. Ctrl+Shift+G looks like Ctrl+G to many programs, so there is no default. | OFF by default | Bind `tab.ungroup` in Preferences > Keybindings |
| Wheel over the strip | on | Nothing: the strip is not a pane. | ON | none |
