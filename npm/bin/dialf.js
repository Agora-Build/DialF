#!/usr/bin/env node
// Thin launcher: exec the vendored native `dialf` binary with the same args.

const { spawnSync } = require('child_process');
const fs = require('fs');
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

const bin = path.join(vendor, dir, 'dialf');
const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
process.exit(r.status === null ? 1 : r.status);
