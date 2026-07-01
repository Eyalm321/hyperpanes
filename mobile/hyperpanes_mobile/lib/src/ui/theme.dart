/// Dark terminal-first theme, echoing the desktop app's palette.
library;

import 'package:flutter/material.dart';
import 'package:xterm/xterm.dart';

const hpBackground = Color(0xFF0D1117);
const hpSurface = Color(0xFF161B22);
const hpBorder = Color(0xFF30363D);
const hpAccent = Color(0xFF3B82F6);
const hpTextDim = Color(0xFF8B949E);

/// Liveness chip colors (dashboard + terminal app bar).
const livenessColors = {
  'working': Color(0xFFF5A623),
  'awaiting-input': Color(0xFFE5484D),
  'done': Color(0xFF30A46C),
  'exited': Color(0xFF6E7681),
};

ThemeData buildTheme() {
  final base = ThemeData.dark(useMaterial3: true);
  return base.copyWith(
    scaffoldBackgroundColor: hpBackground,
    colorScheme: base.colorScheme.copyWith(
      primary: hpAccent,
      surface: hpSurface,
    ),
    appBarTheme: const AppBarTheme(
      backgroundColor: hpSurface,
      elevation: 0,
    ),
    cardTheme: const CardThemeData(
      color: hpSurface,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.all(Radius.circular(10)),
        side: BorderSide(color: hpBorder),
      ),
    ),
  );
}

/// xterm theme matching the desktop defaults.
const hpTerminalTheme = TerminalTheme(
  cursor: Color(0xFF58A6FF),
  selection: Color(0x4058A6FF),
  foreground: Color(0xFFE6EDF3),
  background: hpBackground,
  black: Color(0xFF484F58),
  red: Color(0xFFFF7B72),
  green: Color(0xFF3FB950),
  yellow: Color(0xFFD29922),
  blue: Color(0xFF58A6FF),
  magenta: Color(0xFFBC8CFF),
  cyan: Color(0xFF39C5CF),
  white: Color(0xFFB1BAC4),
  brightBlack: Color(0xFF6E7681),
  brightRed: Color(0xFFFFA198),
  brightGreen: Color(0xFF56D364),
  brightYellow: Color(0xFFE3B341),
  brightBlue: Color(0xFF79C0FF),
  brightMagenta: Color(0xFFD2A8FF),
  brightCyan: Color(0xFF56D4DD),
  brightWhite: Color(0xFFF0F6FC),
  searchHitBackground: Color(0xFFD29922),
  searchHitBackgroundCurrent: Color(0xFFF5A623),
  searchHitForeground: Color(0xFF0D1117),
);

/// Parse the host's `#rrggbb` pane/project colors (fallback: accent).
Color parseHexColor(String? hex) {
  if (hex == null || hex.isEmpty) return hpAccent;
  final h = hex.replaceFirst('#', '');
  final v = int.tryParse(h, radix: 16);
  if (v == null) return hpAccent;
  return h.length == 6 ? Color(0xFF000000 | v) : Color(v);
}
