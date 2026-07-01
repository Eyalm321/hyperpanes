/// The quick-keys bar above the keyboard: chips for keys a phone keyboard lacks.
/// Each chip maps to the control API's named-`keys` vocabulary
/// (`rs/crates/core/src/control/input.rs::keys_to_bytes`).
library;

class QuickKey {
  const QuickKey(this.label, this.keys, {this.hold = false});

  /// Chip text.
  final String label;

  /// Control-API key names sent on tap.
  final List<String> keys;

  /// Sticky modifier styling (reserved; v1 sends discrete chords).
  final bool hold;
}

/// Default bar — coding + Claude driving first.
const defaultQuickKeys = [
  QuickKey('esc', ['escape']),
  QuickKey('tab', ['tab']),
  QuickKey('⇧tab', ['shift+tab']),
  QuickKey('^c', ['ctrl+c']),
  QuickKey('↑', ['up']),
  QuickKey('↓', ['down']),
  QuickKey('←', ['left']),
  QuickKey('→', ['right']),
  QuickKey('pgup', ['pageup']),
  QuickKey('pgdn', ['pagedown']),
  QuickKey('^r', ['ctrl+r']),
  QuickKey('^d', ['ctrl+d']),
  QuickKey('^z', ['ctrl+z']),
  QuickKey('^l', ['ctrl+l']),
];

/// One-tap replies for a Claude pane that's awaiting input.
const claudeQuickReplies = [
  QuickKey('1', []),
  QuickKey('2', []),
  QuickKey('3', []),
  QuickKey('y', []),
  QuickKey('n', []),
];
