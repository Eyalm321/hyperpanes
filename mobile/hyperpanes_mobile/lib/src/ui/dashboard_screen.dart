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

  Future<void> _newPane(int windowId) async {
    // Projects for the picker; a fetch failure just means no dropdown.
    List<ProjectInfo> projects = const [];
    try {
      projects = await session.client.getProjects();
    } catch (_) {}
    if (!mounted) return;
    final spec = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (_) => _NewPaneDialog(projects: projects),
    );
    if (spec == null || !mounted) return;
    final messenger = ScaffoldMessenger.of(context);
    try {
      final res = await session.client.command(
          {'type': 'newPane', 'windowId': windowId, 'pane': spec});
      await session.refresh();
      // newPane returns {ok, result: "<paneId>"} — jump straight in.
      final paneId = res['result'];
      if (paneId is String && mounted) {
        final pane = session.state.paneById(paneId);
        if (pane != null) _openPane(pane);
      }
    } catch (e) {
      messenger.showSnackBar(SnackBar(content: Text('New pane failed: $e')));
    }
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
                            padding: const EdgeInsets.fromLTRB(4, 4, 0, 0),
                            child: Row(
                              children: [
                                Expanded(
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
                                IconButton(
                                  icon: const Icon(Icons.add,
                                      size: 18, color: hpTextDim),
                                  tooltip: 'New pane here',
                                  visualDensity: VisualDensity.compact,
                                  onPressed: () => _newPane(w.windowId),
                                ),
                              ],
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

/// New-pane dialog: quick presets (Shell / Claude), optional custom command, and a
/// project picker (opens in the project's cwd with its color). Pops the `pane` spec
/// for `{type: "newPane"}` or null on cancel.
class _NewPaneDialog extends StatefulWidget {
  const _NewPaneDialog({required this.projects});

  final List<ProjectInfo> projects;

  @override
  State<_NewPaneDialog> createState() => _NewPaneDialogState();
}

class _NewPaneDialogState extends State<_NewPaneDialog> {
  final _commandCtl = TextEditingController();
  String? _project;

  @override
  void dispose() {
    _commandCtl.dispose();
    super.dispose();
  }

  void _create({String? command}) {
    final custom = _commandCtl.text.trim();
    final cmd = command ?? (custom.isEmpty ? null : custom);
    Navigator.of(context).pop(<String, dynamic>{
      'command': ?cmd,
      'project': ?_project,
    });
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('New pane'),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (widget.projects.isNotEmpty)
            DropdownButtonFormField<String?>(
              initialValue: _project,
              decoration: const InputDecoration(labelText: 'Project'),
              items: [
                const DropdownMenuItem(value: null, child: Text('(none)')),
                for (final p in widget.projects)
                  DropdownMenuItem(value: p.name, child: Text(p.name)),
              ],
              onChanged: (v) => setState(() => _project = v),
            ),
          TextField(
            controller: _commandCtl,
            decoration: const InputDecoration(
              labelText: 'Command (empty = shell)',
              hintText: 'claude --continue',
            ),
            autocorrect: false,
            onSubmitted: (_) => _create(),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              Expanded(
                child: OutlinedButton(
                  onPressed: () => _create(command: null),
                  child: const Text('Shell'),
                ),
              ),
              const SizedBox(width: 8),
              Expanded(
                child: OutlinedButton(
                  onPressed: () => _create(command: 'claude'),
                  child: const Text('Claude'),
                ),
              ),
            ],
          ),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _create, child: const Text('Create')),
      ],
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
