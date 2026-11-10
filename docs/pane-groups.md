# Pane groups

Select several panes at once and turn them into one coloured tab. Grouping is reversible.

## Selecting panes

- **Ctrl+click** a pane's header (**Cmd+click** on macOS) to add the pane to the selection or
  remove it. The first Ctrl+click also selects the pane that already has focus. Clicks inside
  terminal content are not affected: Ctrl+click there still copies a clicked path.
- A selection is either empty or has at least 2 panes. Dropping to one pane clears it.
- Selected panes get a 2px outline (its own colour, distinct from the focus ring and the
  project frame), an 8% header tint and a check badge before the title. A hint chip at the end
  of the tab bar (top strip and docked columns) reads "N selected · Ctrl+G group · Esc clear".
- The selection clears on: Esc, a plain click on any pane, switching tabs, any tab-strip
  action, starting a pane drag, grouping, and when a selected pane closes (re-checked against
  the 2-pane minimum).
- Not selectable: panes in remote tabs. Only panes of the active tab can be selected.
- Without a mouse, use the command palette: "Toggle selection of focused pane" (the first
  pane is only *armed* until a second one joins), "Group selected panes", "Clear pane
  selection" and "Ungroup tab".

## Keys

| Key | Action | When |
|-----|--------|------|
| Ctrl+G | Group the selection | only while 2+ panes are selected; otherwise it reaches the pane as BEL |
| Esc | Clear the selection | only while 2+ panes are selected; otherwise it reaches the pane unchanged |
| (unbound) | Ungroup tab, Toggle selection of focused pane | bind in Preferences > Keybindings |

Both guarded chords are rebindable; Preferences shows them as active "when 2+ panes are
selected". See the decision table in [tab-bar.md](tab-bar.md).

## Menus

- Right-click a **selected** pane: "N panes selected" (disabled), "Group N panes", "Clear
  selection", then the usual pane menu. Right-clicking never groups by itself.
- Right-click an **unselected** pane: clears the selection and shows the usual menu.
- A **group tab** adds "Ungroup" to its pane menu and tab menu; the tab menu also has "Group
  Color" (every known project, plus "Use project colour").
- None of these appear in remote tabs.

## What a group is

- **Group** moves the selected panes (in their current order) into a new tab right after the
  source tab, with the Automatic layout, switches to it and focuses the clicked or focused
  pane. If every pane of the tab is selected, that tab simply becomes the group in place. A
  zoomed pane is un-zoomed first.
- The tab is titled with the project name (or "Group") and can be renamed.
- It shows a 3px stripe, a 10% tint and a small group glyph in the group colour, in the top
  strip and in left/right docked tab columns. Its accessible label is
  "<title>, pane group[, <project>]".
- **Colour** (first match wins): the project shared by every pane; the clicked or focused
  pane's project; the first pane with a project; neutral. A pane's project is the project
  whose git root contains its working directory. Recolouring the project recolours the tab.
  A manual tab colour set over the control API (`recolorTab`) overrides the group colour until
  it is cleared (`"color": null`) or the user picks a "Group Color". See
  [tab-bar.md](tab-bar.md#tab-colour-and-the-control-api).
- **Ungroup** sends the panes back to the tab they came from, if it is still open, and closes
  the group tab. Otherwise the tab just stops being a group. Ungroup never closes a pane.
- The origin of a group is runtime-only; after a restart a group tab ungroups into an ordinary
  tab.

## Persistence

A tab in the session/workspace file gains an optional `"paneGroup": {"project": "...",
"color": "#rrggbb"}`. Older versions ignore it and see an ordinary tab. The selection itself
is not saved. See [hyperpanes-format.md](hyperpanes-format.md).

## Control API

- `GET /state`: every tab carries `paneGroup` or `null`, next to the tab's manual `color`
  (`recolorTab`, or `null`).
- `POST /command {"type":"groupPanes","paneIds":[...],"title"?,"project"?}` returns
  `{"tabId"}`; the panes must be local, in one window, at least 2 (they may come from
  different tabs).
- `POST /command {"type":"ungroupPanes","tabId":"..."}`.

## Out of scope

Broadcasting input to a group, in-tab groups that move or resize together, Shift+click range
selection, dragging several panes at once.
