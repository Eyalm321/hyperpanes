import { useEffect, useRef, useState } from 'react';
import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { useKeybindings } from '../store/useKeybindings';
import { useSettings } from '../store/useSettings';
import { comboLabel } from '../keybindings';
import { AUTO_LAYOUT, LAYOUTS, effectiveLayout } from '../layout/presets';
import { serializeWorkspace } from '../workspace/serialize';
import {
  IconMenu,
  IconSettings,
  IconOpen,
  IconSave,
  IconCommands,
  IconPlus,
  IconMinimize,
  IconMaximize,
  IconRestore,
  IconClose
} from './Icons';
import { TabStrip } from './TabStrip';

export function TopBar() {
  const layout = useWorkspace((s) => activeGroup(s).layout);
  const paneCount = useWorkspace((s) => activeGroup(s).panes.length);
  const setLayout = useWorkspace((s) => s.setLayout);
  const openNewPane = useUI((s) => s.openNewPane);
  const openPalette = useUI((s) => s.openPalette);
  const openPreferences = useUI((s) => s.openPreferences);
  const showSidebar = useSettings((s) => s.showSidebar);
  const setShowSidebar = useSettings((s) => s.setShowSidebar);
  const paletteKey = useKeybindings((s) => comboLabel(s.combos['palette.toggle']));

  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  // Close the application menu on outside click or Escape.
  useEffect(() => {
    if (!menuOpen) return;
    const onDown = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenuOpen(false);
    };
    document.addEventListener('mousedown', onDown, true);
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('keydown', onKey, true);
    };
  }, [menuOpen]);

  const [maximized, setMaximized] = useState(false);
  useEffect(() => {
    let active = true;
    void window.hp.win.isMaximized().then((m) => active && setMaximized(m));
    const off = window.hp.win.onMaximizeChange(setMaximized);
    return () => {
      active = false;
      off();
    };
  }, []);

  const saveWorkspace = () => void window.hp.workspace.save(serializeWorkspace());
  const openWorkspace = async () => {
    const file = await window.hp.workspace.open();
    if (file && Array.isArray(file.panes) && file.panes.length > 0) {
      useWorkspace.getState().loadWorkspace(file);
    }
  };

  // Run a menu action and dismiss the menu.
  const fromMenu = (fn: () => void) => () => {
    setMenuOpen(false);
    fn();
  };

  // Current layout's label/glyph for the Layout header, plus the preset 'auto'
  // resolves to right now (for the "Automatic — grid" hint).
  const current = layout === 'auto' ? AUTO_LAYOUT : LAYOUTS.find((l) => l.id === layout);
  const autoResolved = LAYOUTS.find((l) => l.id === effectiveLayout('auto', paneCount));

  return (
    <div className="hp-topbar">
      <div className="hp-menu-wrap" ref={menuRef}>
        <button
          className={`hp-iconbtn${menuOpen ? ' active' : ''}`}
          onClick={() => setMenuOpen((o) => !o)}
          title="Menu"
          aria-label="Menu"
          aria-haspopup="menu"
          aria-expanded={menuOpen}
        >
          <IconMenu />
        </button>
        {menuOpen && (
          <div className="hp-menu" role="menu">
            <button className="hp-menu-item" role="menuitem" onClick={fromMenu(openNewPane)}>
              <IconPlus />
              <span>New pane…</span>
            </button>
            <button className="hp-menu-item" role="menuitem" onClick={fromMenu(openPalette)}>
              <IconCommands />
              <span>Command palette</span>
              <span className="hp-menu-shortcut">{paletteKey}</span>
            </button>
            <div className="hp-menu-sep" />
            {/* Classic cascading submenu: hovering "Layout" flies the presets out
                to the right (CSS :hover/:focus-within on .hp-has-submenu). */}
            <div className="hp-menu-item hp-has-submenu" role="menuitem" aria-haspopup="true" tabIndex={0}>
              <span className="hp-menu-glyph">{current?.icon}</span>
              <span>Layout</span>
              <span className="hp-menu-shortcut">{current?.label} ▸</span>
              <div className="hp-submenu" role="menu">
                <button
                  className={`hp-menu-item${layout === 'auto' ? ' active' : ''}`}
                  role="menuitemradio"
                  aria-checked={layout === 'auto'}
                  onClick={() => setLayout('auto')}
                >
                  <span className="hp-menu-glyph">{AUTO_LAYOUT.icon}</span>
                  <span>{AUTO_LAYOUT.label}</span>
                  {autoResolved && <span className="hp-menu-hint">— {autoResolved.label}</span>}
                  <span className="hp-menu-radio">{layout === 'auto' ? '✓' : ''}</span>
                </button>
                <div className="hp-menu-sep" />
                {LAYOUTS.map((l) => (
                  <button
                    key={l.id}
                    className={`hp-menu-item${layout === l.id ? ' active' : ''}`}
                    role="menuitemradio"
                    aria-checked={layout === l.id}
                    onClick={() => setLayout(l.id)}
                  >
                    <span className="hp-menu-glyph">{l.icon}</span>
                    <span>{l.label}</span>
                    <span className="hp-menu-radio">{layout === l.id ? '✓' : ''}</span>
                  </button>
                ))}
              </div>
            </div>
            <div className="hp-menu-sep" />
            <button className="hp-menu-item" role="menuitem" onClick={fromMenu(openWorkspace)}>
              <IconOpen />
              <span>Open workspace…</span>
            </button>
            <button className="hp-menu-item" role="menuitem" onClick={fromMenu(saveWorkspace)}>
              <IconSave />
              <span>Save workspace…</span>
            </button>
            <div className="hp-menu-sep" />
            <button
              className="hp-menu-item"
              role="menuitemcheckbox"
              aria-checked={showSidebar}
              onClick={fromMenu(() => setShowSidebar(!showSidebar))}
            >
              <span className="hp-menu-glyph">▥</span>
              <span>Sidebar</span>
              <span className="hp-menu-radio">{showSidebar ? '✓' : ''}</span>
            </button>
            <button className="hp-menu-item" role="menuitem" onClick={fromMenu(openPreferences)}>
              <IconSettings />
              <span>Preferences…</span>
            </button>
          </div>
        )}
      </div>

      {/* The tab strip fills the middle and doubles as the window-drag region
          (its empty trailing area stays app-region: drag). Replaces the old spacer. */}
      <TabStrip />

      <span className="hp-winsep" />

      <div className="hp-wincontrols">
        <button
          className="hp-winbtn"
          onClick={() => window.hp.win.minimize()}
          title="Minimize"
          aria-label="Minimize"
        >
          <IconMinimize />
        </button>
        <button
          className="hp-winbtn"
          onClick={() => window.hp.win.toggleMaximize()}
          title={maximized ? 'Restore' : 'Maximize'}
          aria-label={maximized ? 'Restore' : 'Maximize'}
        >
          {maximized ? <IconRestore /> : <IconMaximize />}
        </button>
        <button
          className="hp-winbtn hp-winclose"
          onClick={() => window.hp.win.close()}
          title="Close"
          aria-label="Close"
        >
          <IconClose />
        </button>
      </div>
    </div>
  );
}
