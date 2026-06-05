// Loads node-pty inside the Electron main process to verify the prebuilt
// binary is ABI-compatible with Electron (N-API binaries should be).
// Run with: npx electron scripts/check-pty.js   (do NOT set ELECTRON_RUN_AS_NODE)
const { app } = require('electron');

function check() {
  try {
    const pty = require('node-pty');
    const ver = require('node-pty/package.json').version;
    const shell = process.platform === 'win32' ? 'cmd.exe' : '/bin/sh';
    const p = pty.spawn(shell, [], { name: 'xterm-256color', cols: 80, rows: 24, cwd: process.cwd(), env: process.env });
    let got = '';
    p.onData((d) => { got += d; });
    setTimeout(() => {
      console.log('PTY_CHECK_RESULT: OK node-pty@' + ver + ' loaded in Electron; received ' + got.length + ' bytes');
      try { p.kill(); } catch {}
      app.exit(0);
    }, 600);
  } catch (err) {
    console.error('PTY_CHECK_RESULT: FAIL ' + (err && err.message ? err.message : String(err)));
    app.exit(2);
  }
}

if (app.isReady()) check();
else app.whenReady().then(check);
