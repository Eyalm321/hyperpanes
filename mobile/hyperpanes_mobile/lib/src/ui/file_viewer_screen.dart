/// Read-only viewer for a host file (tap-a-path in the terminal). Monospace, line
/// numbers, jumps to a `path:line` target, share-to-clipboard.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../api/control_client.dart';
import 'theme.dart';

class FileViewerScreen extends StatefulWidget {
  const FileViewerScreen({
    super.key,
    required this.client,
    required this.path,
    this.line,
  });

  final ControlClient client;
  final String path;
  final int? line;

  @override
  State<FileViewerScreen> createState() => _FileViewerScreenState();
}

class _FileViewerScreenState extends State<FileViewerScreen> {
  HostFile? file;
  String? error;
  final _scroll = ScrollController();
  static const _lineHeight = 20.0;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final f = await widget.client.readFile(widget.path);
      if (!mounted) return;
      setState(() => file = f);
      final target = widget.line;
      if (target != null && target > 3) {
        // Jump so the target line sits a few rows below the top.
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (_scroll.hasClients) {
            _scroll.jumpTo(
              ((target - 3) * _lineHeight)
                  .clamp(0, _scroll.position.maxScrollExtent),
            );
          }
        });
      }
    } on ControlApiException catch (e) {
      if (!mounted) return;
      setState(() {
        error = e.status == 404 && e.message.contains('not found')
            ? 'File not found on host'
            : e.status == 404
                ? 'Host app too old — restart it on the new build'
                : e.message;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => error = '$e');
    }
  }

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final name = widget.path.split('/').last;
    return Scaffold(
      appBar: AppBar(
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(name, style: const TextStyle(fontSize: 16)),
            Text(
              widget.path,
              style: const TextStyle(fontSize: 11, color: hpTextDim),
              overflow: TextOverflow.ellipsis,
            ),
          ],
        ),
        actions: [
          IconButton(
            icon: const Icon(Icons.copy_all_outlined),
            tooltip: 'Copy contents',
            onPressed: file == null
                ? null
                : () {
                    Clipboard.setData(ClipboardData(text: file!.content));
                    ScaffoldMessenger.of(context).showSnackBar(
                      const SnackBar(content: Text('Copied to clipboard')),
                    );
                  },
          ),
        ],
      ),
      body: error != null
          ? Center(
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: Text(error!, style: const TextStyle(color: hpTextDim)),
              ),
            )
          : file == null
              ? const Center(child: CircularProgressIndicator())
              : _FileBody(
                  file: file!,
                  highlight: widget.line,
                  controller: _scroll,
                  lineHeight: _lineHeight,
                ),
    );
  }
}

class _FileBody extends StatelessWidget {
  const _FileBody({
    required this.file,
    required this.highlight,
    required this.controller,
    required this.lineHeight,
  });

  final HostFile file;
  final int? highlight;
  final ScrollController controller;
  final double lineHeight;

  @override
  Widget build(BuildContext context) {
    final lines = file.content.split('\n');
    final gutterWidth = '${lines.length}'.length * 9.0 + 16;
    return Column(
      children: [
        if (file.truncated)
          Container(
            width: double.infinity,
            color: hpSurface,
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
            child: Text(
              'Showing first ${file.content.length} chars of ${file.size} bytes',
              style: const TextStyle(fontSize: 12, color: hpTextDim),
            ),
          ),
        Expanded(
          child: ListView.builder(
            controller: controller,
            itemExtent: lineHeight,
            itemCount: lines.length,
            itemBuilder: (context, i) {
              final isTarget = highlight != null && i == highlight! - 1;
              return Container(
                color: isTarget
                    ? hpAccent.withValues(alpha: 0.18)
                    : Colors.transparent,
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    SizedBox(
                      width: gutterWidth,
                      child: Text(
                        '${i + 1}',
                        textAlign: TextAlign.right,
                        style: const TextStyle(
                          fontFamily: 'monospace',
                          fontSize: 12,
                          color: hpTextDim,
                        ),
                      ),
                    ),
                    const SizedBox(width: 10),
                    Expanded(
                      child: SingleChildScrollView(
                        scrollDirection: Axis.horizontal,
                        child: Text(
                          lines[i],
                          maxLines: 1,
                          style: const TextStyle(
                            fontFamily: 'monospace',
                            fontSize: 13,
                          ),
                        ),
                      ),
                    ),
                  ],
                ),
              );
            },
          ),
        ),
      ],
    );
  }
}
