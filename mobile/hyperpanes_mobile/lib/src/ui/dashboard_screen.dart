/// Host dashboard: windows→tabs→panes as live cards (label, project color, AI subtitle,
/// liveness chip). Output-free by design — liveness/state frames only.
library;

import 'dart:async';

import 'package:flutter/material.dart';

import '../api/control_client.dart';
import '../api/models.dart';
import '../api/pairing.dart';
import '../state/host_session.dart';
import 'terminal_screen.dart';
import 'theme.dart';

class DashboardScreen extends StatefulWidget {
  const DashboardScreen({super.key, required this.pairing});

  final HostPairing pairing;

  @override
  State<DashboardScreen> createState() => _DashboardScreenState();
}

class _DashboardScreenState extends State<DashboardScreen> {
  late final HostSession session;

  @override
  void initState() {
    super.initState();
    session = HostSession(widget.pairing);
    unawaited(session.start());
    session.awaitingInputPane.addListener(_onAwaitingInput);
  }

  void _onAwaitingInput() {
    final paneId = session.awaitingInputPane.value;
    if (paneId == null || !mounted) return;
    final pane = session.state.paneById(paneId);
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text('${pane?.label ?? paneId} is waiting for input'),
        action: SnackBarAction(
          label: 'Open',
          onPressed: () {
            if (pane != null) _openPane(pane);
          },
        ),
      ),
    );
  }

  @override
  void dispose() {
    session.awaitingInputPane.removeListener(_onAwaitingInput);
    session.dispose();
    super.dispose();
  }

  void _openPane(PaneInfo pane) {
    Navigator.of(context).push(
      MaterialPageRoute(
        builder: (_) => TerminalScreen(session: session, paneId: pane.id),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return AnimatedBuilder(
      animation: session,
      builder: (context, _) {
        final s = session.state;
        return Scaffold(
          appBar: AppBar(
            title: Text(widget.pairing.displayName),
            actions: [
              ValueListenableBuilder(
                valueListenable: session.events.state,
                builder: (_, st, child) => Padding(
                  padding: const EdgeInsets.only(right: 12),
                  child: Icon(
                    Icons.circle,
                    size: 10,
                    color: switch (st) {
                      EventsState.connected => livenessColors['done'],
                      EventsState.connecting => livenessColors['working'],
                      EventsState.disconnected =>
                        livenessColors['awaiting-input'],
                    },
                  ),
                ),
              ),
            ],
          ),
          body: RefreshIndicator(
            onRefresh: session.refresh,
            child: session.lastError != null && s.windows.isEmpty
                ? ListView(
                    children: [
                      Padding(
                        padding: const EdgeInsets.all(24),
                        child: Text(
                          'Cannot reach host:\n${session.lastError}',
                          style: const TextStyle(color: hpTextDim),
                        ),
                      ),
                    ],
                  )
                : ListView(
                    padding: const EdgeInsets.all(12),
                    children: [
                      for (final w in s.windows)
                        for (final t in w.tabs) ...[
                          Padding(
                            padding: const EdgeInsets.fromLTRB(4, 12, 4, 6),
                            child: Text(
                              s.windows.length > 1
                                  ? 'window ${w.windowId} · ${t.title.isEmpty ? t.id : t.title}'
                                  : (t.title.isEmpty ? t.id : t.title),
                              style: Theme.of(context)
                                  .textTheme
                                  .labelLarge
                                  ?.copyWith(color: hpTextDim),
                            ),
                          ),
                          for (final p in t.panes)
                            _PaneCard(
                              pane: p,
                              liveness: session.liveness[p.id] ?? 'done',
                              onTap: () => _openPane(p),
                            ),
                        ],
                      const SizedBox(height: 48),
                    ],
                  ),
          ),
        );
      },
    );
  }
}

class _PaneCard extends StatelessWidget {
  const _PaneCard({
    required this.pane,
    required this.liveness,
    required this.onTap,
  });

  final PaneInfo pane;
  final String liveness;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final chipColor = livenessColors[liveness] ?? hpTextDim;
    final subtitle = pane.aiSubtitle ?? pane.cwd ?? pane.command ?? '';
    return Card(
      margin: const EdgeInsets.symmetric(vertical: 4),
      child: ListTile(
        onTap: onTap,
        leading: Container(
          width: 6,
          height: 40,
          decoration: BoxDecoration(
            color: parseHexColor(pane.color),
            borderRadius: BorderRadius.circular(3),
          ),
        ),
        title: Row(
          children: [
            Expanded(
              child: Text(pane.label.isEmpty ? pane.id : pane.label,
                  overflow: TextOverflow.ellipsis),
            ),
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
              decoration: BoxDecoration(
                color: chipColor.withValues(alpha: 0.15),
                borderRadius: BorderRadius.circular(10),
              ),
              child: Text(
                liveness,
                style: TextStyle(fontSize: 11, color: chipColor),
              ),
            ),
          ],
        ),
        subtitle: subtitle.isEmpty
            ? null
            : Text(
                subtitle,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(color: hpTextDim, fontSize: 12),
              ),
      ),
    );
  }
}
