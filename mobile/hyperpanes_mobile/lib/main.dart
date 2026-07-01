import 'package:flutter/material.dart';

import 'src/ui/connect_screen.dart';
import 'src/ui/theme.dart';

void main() {
  runApp(const HyperpanesApp());
}

class HyperpanesApp extends StatelessWidget {
  const HyperpanesApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'hyperpanes',
      theme: buildTheme(),
      debugShowCheckedModeBanner: false,
      home: const ConnectScreen(),
    );
  }
}
