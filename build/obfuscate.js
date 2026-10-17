'use strict';
// Writes an obfuscated copy of the interface to frontend/dist, which release
// builds embed instead of the readable files (go build / wails build -tags obf).
// The Go code itself is compiled to machine code and needs no extra step.
//   node build/obfuscate.js
const fs = require('fs');
const path = require('path');
const JavaScriptObfuscator = require('javascript-obfuscator');

const src = path.join(__dirname, '..', 'frontend');
const out = path.join(src, 'dist');

const options = {
  compact: true,
  target: 'browser',
  identifierNamesGenerator: 'hexadecimal',
  stringArray: true,
  stringArrayEncoding: ['base64'],
  stringArrayThreshold: 0.75,
  splitStrings: true,
  splitStringsChunkLength: 8,
  // Light enough that typing and scrolling stay smooth.
  controlFlowFlattening: true,
  controlFlowFlatteningThreshold: 0.15,
  // The scripts share top-level names (S, Voice, store...) and the desktop
  // app calls a few of them by name, so globals keep their names.
  renameGlobals: false,
  selfDefending: false,
  sourceMap: false,
};

fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out);
for (const name of fs.readdirSync(src)) {
  const file = path.join(src, name);
  if (!fs.statSync(file).isFile() || name.endsWith('.go')) continue;
  if (name.endsWith('.js') && name !== 'config.js') {
    const code = fs.readFileSync(file, 'utf8');
    fs.writeFileSync(path.join(out, name), JavaScriptObfuscator.obfuscate(code, options).getObfuscatedCode());
    console.log(`  ${name} ofuscado`);
  } else {
    fs.copyFileSync(file, path.join(out, name));
  }
}
