// postinstall: download the prebuilt dialf binary (+ bundled ten-vad lib) for this
// platform into vendor/. Does NOT install the service — run `sudo dialf service install`
// (or `dialf service install --user`) afterward. Never hard-fails `npm install`.

const https = require('https');
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const pkg = require('./package.json');
const REPO = process.env.DIALF_REPO || 'Agora-Build/DialF';
const version = pkg.version;
const tag = 'v' + version;

const ARCH = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
const OS = { darwin: 'darwin', linux: 'linux' }[process.platform];

function bail(msg) {
  // Print guidance but exit 0 so `npm install` doesn't fail the whole tree.
  console.error('dialf: ' + msg);
  console.error('dialf: download/build manually from https://github.com/' + REPO + '/releases');
  process.exit(0);
}

if (!OS || !ARCH) bail('unsupported platform ' + process.platform + '/' + process.arch);

const target = `${OS}-${ARCH}`;
const asset = `dialf-${version}-${target}.tar.gz`;
const url = `https://github.com/${REPO}/releases/download/${tag}/${asset}`;
const vendor = path.join(__dirname, 'vendor');

function download(u, dest, cb) {
  https
    .get(u, { headers: { 'User-Agent': 'dialf-npm' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return download(res.headers.location, dest, cb);
      }
      if (res.statusCode !== 200) return cb(new Error('HTTP ' + res.statusCode + ' for ' + u));
      const f = fs.createWriteStream(dest);
      res.pipe(f);
      f.on('finish', () => f.close(() => cb(null)));
    })
    .on('error', cb);
}

fs.mkdirSync(vendor, { recursive: true });
const tgz = path.join(vendor, asset);
console.log('dialf: downloading ' + url);
download(url, tgz, (err) => {
  if (err) return bail('download failed: ' + err.message);
  try {
    execSync(`tar -xzf "${tgz}" -C "${vendor}"`);
    fs.unlinkSync(tgz);
  } catch (e) {
    return bail('extract failed: ' + e.message);
  }
  console.log('dialf: installed ' + target);
  console.log('dialf: launch dialfd:  dialf daemon  |  sudo dialf service install  (boot)');
  console.log('dialf:   per-user (login; required on macOS for audio recording):  dialf service install --user');
});
