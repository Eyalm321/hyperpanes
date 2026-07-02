/// Read-model types mirroring the host's `GET /state` JSON
/// (`rs/crates/core/src/control/readmodel.rs::PaneOut/TabOut/WindowOut/StateOut`).
///
/// Parsing is tolerant: unknown fields ignored, missing optionals null — the host adds
/// fields additively, so an older app must keep working against a newer host.
library;

class PaneInfo {
  const PaneInfo({
    required this.id,
    required this.sessionUid,
    required this.label,
    required this.color,
    required this.status,
    required this.activity,
    this.subtitle,
    this.command,
    this.args,
    this.cwd,
    this.shell,
    this.exitCode,
    this.meta,
    this.cols,
    this.rows,
  });

  final String id;
  final String sessionUid;
  final String label;
  final String color;

  /// `running | exited`.
  final String status;

  /// Frozen legacy liveness: `busy | idle | exited`. The precise value
  /// (`working | awaiting-input | done | exited`) rides `liveness` WS frames.
  final String activity;

  final String? subtitle;
  final String? command;
  final List<String>? args;
  final String? cwd;
  final String? shell;
  final int? exitCode;
  final Map<String, String>? meta;

  /// Host grid dims — what our emulator must resize to. Null on daemon-backed panes.
  final int? cols;
  final int? rows;

  /// Heuristic: is this pane running a Claude/agent CLI? Drives the composer UI.
  bool get looksLikeAgent {
    final c = (command ?? '').toLowerCase();
    if (c.contains('claude') || c.contains('codex') || c.contains('agent')) {
      return true;
    }
    return meta?.containsKey('ai.subtitle') ?? false;
  }

  String? get aiSubtitle => subtitle ?? meta?['ai.subtitle'];

  static PaneInfo? fromJson(Map<String, dynamic> j) {
    final id = j['id'];
    final uid = j['sessionUid'];
    if (id is! String || uid is! String) return null;
    return PaneInfo(
      id: id,
      sessionUid: uid,
      label: j['label'] as String? ?? '',
      color: j['color'] as String? ?? '',
      status: j['status'] as String? ?? 'running',
      activity: j['activity'] as String? ?? 'idle',
      subtitle: j['subtitle'] as String?,
      command: j['command'] as String?,
      args: (j['args'] as List?)?.whereType<String>().toList(),
      cwd: j['cwd'] as String?,
      shell: j['shell'] as String?,
      exitCode: j['exitCode'] as int?,
      meta: (j['meta'] as Map?)?.map(
        (k, v) => MapEntry(k.toString(), v.toString()),
      ),
      cols: j['cols'] as int?,
      rows: j['rows'] as int?,
    );
  }
}

class TabInfo {
  const TabInfo({
    required this.id,
    required this.title,
    required this.layout,
    required this.panes,
  });

  final String id;
  final String title;
  final String layout;
  final List<PaneInfo> panes;

  static TabInfo? fromJson(Map<String, dynamic> j) {
    final id = j['id'];
    if (id is! String) return null;
    return TabInfo(
      id: id,
      title: j['title'] as String? ?? '',
      layout: j['layout'] as String? ?? '',
      panes: (j['panes'] as List? ?? [])
          .whereType<Map<String, dynamic>>()
          .map(PaneInfo.fromJson)
          .whereType<PaneInfo>()
          .toList(),
    );
  }
}

class WindowInfo {
  const WindowInfo({
    required this.windowId,
    required this.tabs,
    this.activeTabId,
  });

  final int windowId;
  final String? activeTabId;
  final List<TabInfo> tabs;

  static WindowInfo? fromJson(Map<String, dynamic> j) {
    return WindowInfo(
      windowId: (j['windowId'] as num?)?.toInt() ?? 0,
      activeTabId: j['activeTabId'] as String?,
      tabs: (j['tabs'] as List? ?? [])
          .whereType<Map<String, dynamic>>()
          .map(TabInfo.fromJson)
          .whereType<TabInfo>()
          .toList(),
    );
  }
}

class HostState {
  const HostState({required this.windows});

  final List<WindowInfo> windows;

  Iterable<PaneInfo> get allPanes =>
      windows.expand((w) => w.tabs).expand((t) => t.panes);

  PaneInfo? paneById(String id) {
    for (final p in allPanes) {
      if (p.id == id) return p;
    }
    return null;
  }

  static HostState fromJson(Map<String, dynamic> j) {
    return HostState(
      windows: (j['windows'] as List? ?? [])
          .whereType<Map<String, dynamic>>()
          .map(WindowInfo.fromJson)
          .whereType<WindowInfo>()
          .toList(),
    );
  }

  static const empty = HostState(windows: []);
}

/// One project from `GET /projects`.
class ProjectInfo {
  const ProjectInfo({required this.name, required this.path, this.color});

  final String name;
  final String path;
  final String? color;

  static ProjectInfo? fromJson(Map<String, dynamic> j) {
    final name = j['name'];
    final path = j['path'];
    if (name is! String || path is! String) return null;
    return ProjectInfo(name: name, path: path, color: j['color'] as String?);
  }
}
