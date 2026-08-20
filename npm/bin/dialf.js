#!/usr/bin/env node
// Thin launcher for the vendored native `dialf` binary — must be TRANSPARENT, so dialf
// behaves identically installed via npm, curl (direct symlink), or built from source.
//
// Not spawnSync: Ctrl+C sends SIGINT to the whole foreground process group — the native
// binary catches it for its multi-level job-cancel flow, but spawnSync leaves this wrapper
// to die on the same SIGINT, which hands the shell its prompt back and orphans the binary
// so further Ctrl+C presses never reach it. Instead: async spawn, stay alive through
// SIGINT (the tty already delivers it to the child), forward direct termination signals,
// and mirror the child's exit code / fatal signal exactly.

const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const vendor = path.join(__dirname, '..', 'vendor');
let dir;
try {
  dir = fs.readdirSync(vendor).find((d) => d.startsWith('dialf-'));
} catch (_) {
  /* vendor missing */
}
if (!dir) {
  console.error('dialf: native binary not found — reinstall (@agora-build/dialf) or build from source.');
  process.exit(1);
}

const child = spawn(path.join(vendor, dir, 'dialf'), process.argv.slice(2), { stdio: 'inherit' });

// Ctrl+C: the child already receives its own SIGINT from the tty — just don't die with it.
process.on('SIGINT', () => {});
// Signals sent to the wrapper alone (kill <wrapper-pid>) are forwarded to the child.
for (const sig of ['SIGTERM', 'SIGHUP']) {
  process.on(sig, () => {
    try {
      child.kill(sig);
    } catch (_) {
      /* already gone */
    }
  });
}

child.on('error', (e) => {
  console.error(`dialf: ${e.message}`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  process.exit(signal ? 128 + (os.constants.signals[signal] || 1) : code === null ? 1 : code);
});
