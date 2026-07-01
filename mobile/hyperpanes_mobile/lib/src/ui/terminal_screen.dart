/// Full-screen terminal for one pane: xterm view locked to the HOST grid (autoResize
/// off + terminal.resize(hostCols, hostRows) — a pty byte stream only renders correctly
/// at the width it was produced for), font auto-fit to screen width, quick-keys bar,
/// and a Claude-style composer for agent panes.
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:xterm/xterm.dart';

import '../api/models.dart';
import '../state/host_session.dart';
import '../term/pane_session.dart';
import '../term/quick_keys.dart';
import 'theme.dart';

class TerminalScreen extends StatefulWidget {
  const TerminalScreen({
    super.key,
    required this.session,
    required this.paneId,
  });

  final HostSession session;
  final String paneId;

  @override
  State<TerminalScreen> createState() => _TerminalScreenState();
}

class _TerminalScreenState extends State<TerminalScreen> {
  PaneSession? pane;
  final _composerCtl = TextEditingController();
  bool _composerMode = false;

  PaneInfo? get info => widget.session.state.paneById(widget.paneId);

  @override
  void initState() {
    super.initState();
    final p = info;
    pane = PaneSession.connect(
      paneId: widget.paneId,
      sessionUid: p?.sessionUid ?? '',
      client: widget.session.client,
      events: widget.session.events,
      cols: p?.cols,
      rows: p?.rows,
    );
    unawaited(pane!.attach());
    _composerMode = p?.looksLikeAgent ?? false;
  }

  @override
  void dispose() {
    pane?.dispose();
    _composerCtl.dispose();
    super.dispose();
  }

  Future<void> _sendComposer() async {
    final text = _composerCtl.text;
    if (text.isEmpty) return;
    _composerCtl.clear();
    await pane!.sendText(text, submit: true);
  }

  @override
  Widget build(BuildContext context) {
    final p = info;
    final liveness = widget.session.liveness[widget.paneId] ?? 'done';
    final chipColor = livenessColors[liveness] ?? hpTextDim;
    return Scaffold(
      appBar: AppBar(
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(p?.label ?? widget.paneId,
                style: const TextStyle(fontSize: 16)),
            if (p?.aiSubtitle != null)
              Text(
                p!.aiSubtitle!,
                style: const TextStyle(fontSize: 11, color: hpTextDim),
                overflow: TextOverflow.ellipsis,
              ),
          ],
        ),
        actions: [
          Center(
            child: Container(
              margin: const EdgeInsets.only(right: 8),
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
              decoration: BoxDecoration(
                color: chipColor.withValues(alpha: 0.15),
                borderRadius: BorderRadius.circular(10),
              ),
              child: Text(liveness,
                  style: TextStyle(fontSize: 11, color: chipColor)),
            ),
          ),
          IconButton(
            icon: Icon(_composerMode ? Icons.keyboard : Icons.chat_outlined),
            tooltip: _composerMode ? 'Raw keyboard' : 'Composer',
            onPressed: () => setState(() => _composerMode = !_composerMode),
          ),
          _ActionsMenu(session: widget.session, paneId: widget.paneId),
        ],
      ),
      body: SafeArea(
        child: Column(
          children: [
            if (liveness == 'awaiting-input')
              _AwaitingInputBanner(pane: pane!),
            Expanded(child: _FittedTerminal(pane: pane!)),
            _QuickKeysBar(pane: pane!),
            if (_composerMode) _composer(),
          ],
        ),
      ),
    );
  }

  Widget _composer() {
    return Container(
      color: hpSurface,
      padding: const EdgeInsets.fromLTRB(8, 4, 8, 8),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.end,
        children: [
          Expanded(
            child: TextField(
              controller: _composerCtl,
              minLines: 1,
              maxLines: 6,
              autocorrect: false,
              style: const TextStyle(fontSize: 14),
              decoration: const InputDecoration(
                hintText: 'Prompt…',
                isDense: true,
                border: OutlineInputBorder(),
              ),
              textInputAction: TextInputAction.newline,
            ),
          ),
          const SizedBox(width: 8),
          IconButton.filled(
            icon: const Icon(Icons.send),
            onPressed: _sendComposer,
          ),
        ],
      ),
    );
  }
}

/// Renders the fixed host grid, scaled so its WIDTH fits the screen; taller grids
/// pan vertically. `autoResize: false` keeps the emulator at host cols×rows no matter
/// what size the widget gets.
class _FittedTerminal extends StatelessWidget {
  const _FittedTerminal({required this.pane});

  final PaneSession pane;

  @override
  Widget build(BuildContext context) {
    // Base cell size at font 14 for JetBrains-class monospace; the FittedBox scale
    // corrects any drift, so this only sets the render resolution.
    const baseFont = 14.0;
    const cellW = baseFont * 0.6;
    const cellH = baseFont * 1.4;
    final gridW = pane.hostCols * cellW;
    final gridH = pane.hostRows * cellH;
    return LayoutBuilder(
      builder: (context, constraints) {
        final scale = (constraints.maxWidth / gridW).clamp(0.2, 2.0);
        return SingleChildScrollView(
          child: SizedBox(
            width: constraints.maxWidth,
            height: gridH * scale,
            child: FittedBox(
              fit: BoxFit.fill,
              child: SizedBox(
                width: gridW,
                height: gridH,
                child: TerminalView(
                  pane.terminal,
                  theme: hpTerminalTheme,
                  autoResize: false,
                  textStyle: const TerminalStyle(fontSize: baseFont),
                  backgroundOpacity: 0,
                  hardwareKeyboardOnly: false,
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

class _QuickKeysBar extends StatelessWidget {
  const _QuickKeysBar({required this.pane});

  final PaneSession pane;

  @override
  Widget build(BuildContext context) {
    return Container(
      color: hpSurface,
      height: 40,
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 4),
        children: [
          for (final k in defaultQuickKeys)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 5),
              child: ActionChip(
                label: Text(k.label,
                    style:
                        const TextStyle(fontSize: 12, fontFamily: 'monospace')),
                visualDensity: VisualDensity.compact,
                onPressed: () => unawaited(
                  pane.sendKeys(k.keys).catchError((_) {}),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// Red banner when the agent is blocked on input: one-tap replies (1/2/3, y/n, Esc).
class _AwaitingInputBanner extends StatelessWidget {
  const _AwaitingInputBanner({required this.pane});

  final PaneSession pane;

  @override
  Widget build(BuildContext context) {
    final red = livenessColors['awaiting-input']!;
    return Container(
      color: red.withValues(alpha: 0.15),
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: Row(
        children: [
          Icon(Icons.pan_tool_alt_outlined, size: 16, color: red),
          const SizedBox(width: 8),
          const Expanded(
            child: Text('Waiting for input', style: TextStyle(fontSize: 12)),
          ),
          for (final r in ['1', '2', '3', 'y', 'n'])
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 2),
              child: ActionChip(
                label: Text(r, style: const TextStyle(fontSize: 12)),
                visualDensity: VisualDensity.compact,
                onPressed: () =>
                    unawaited(pane.sendText(r, submit: true).catchError((_) {})),
              ),
            ),
          ActionChip(
            label: const Text('esc', style: TextStyle(fontSize: 12)),
            visualDensity: VisualDensity.compact,
            onPressed: () =>
                unawaited(pane.sendKeys(['escape']).catchError((_) {})),
          ),
        ],
      ),
    );
  }
}

class _ActionsMenu extends StatelessWidget {
  const _ActionsMenu({required this.session, required this.paneId});

  final HostSession session;
  final String paneId;

  @override
  Widget build(BuildContext context) {
    return PopupMenuButton<String>(
      onSelected: (v) async {
        final messenger = ScaffoldMessenger.of(context);
        final navigator = Navigator.of(context);
        try {
          switch (v) {
            case 'focus':
              await session.client
                  .command({'type': 'focusPane', 'paneId': paneId});
            case 'restart':
              await session.client
                  .command({'type': 'restartPane', 'paneId': paneId});
            case 'close':
              await session.client
                  .command({'type': 'closePane', 'paneId': paneId});
              navigator.pop();
            case 'rename':
              final name = await _prompt(context, 'Rename pane');
              if (name != null && name.isNotEmpty) {
                await session.client.command(
                    {'type': 'renamePane', 'paneId': paneId, 'label': name});
              }
          }
        } catch (e) {
          messenger.showSnackBar(SnackBar(content: Text('$e')));
        }
      },
      itemBuilder: (_) => const [
        PopupMenuItem(value: 'focus', child: Text('Focus on host')),
        PopupMenuItem(value: 'rename', child: Text('Rename')),
        PopupMenuItem(value: 'restart', child: Text('Restart')),
        PopupMenuItem(value: 'close', child: Text('Close pane')),
      ],
    );
  }

  Future<String?> _prompt(BuildContext context, String title) {
    final ctl = TextEditingController();
    return showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(title),
        content: TextField(controller: ctl, autofocus: true),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(ctl.text),
            child: const Text('OK'),
          ),
        ],
      ),
    );
  }
}
