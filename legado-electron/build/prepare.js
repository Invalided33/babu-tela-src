'use strict';
// Makes the obfuscated copy of the app that goes into the installer:
//   app/ and server/  ->  dist-src/app and dist-src/server
// `npm run dist` runs this before electron-builder (local build: the app hosts
// its own server). `npm run dist:public` passes --public: the app connects to
// "babuTela.publicServerUrl" from package.json and the server code stays out.
// The source folders are never changed, so `npm run app` and `npm run server`
// keep using readable code.
const fs = require('fs');
const path = require('path');
const JavaScriptObfuscator = require('javascript-obfuscator');

const root = path.join(__dirname, '..');
const out = path.join(root, 'dist-src');

const common = {
  compact: true,
  identifierNamesGenerator: 'hexadecimal',
  stringArray: true,
  stringArrayEncoding: ['base64'],
  stringArrayThreshold: 0.75,
  splitStrings: true,
  splitStringsChunkLength: 8,
  transformObjectKeys: false,
  // The renderer's two scripts share top-level names (S, Voice, store...),
  // and main.js calls renderer globals by name, so globals keep their names.
  renameGlobals: false,
  selfDefending: false,
  sourceMap: false,
};
const PROFILES = {
  node: { ...common, target: 'node', controlFlowFlattening: true, controlFlowFlatteningThreshold: 0.5 },
  // Lighter on the UI so typing and scrolling stay smooth.
  browser: { ...common, target: 'browser', controlFlowFlattening: true, controlFlowFlatteningThreshold: 0.15 },
};

function obfuscate(src, dest, profile) {
  const code = fs.readFileSync(src, 'utf8');
  const result = JavaScriptObfuscator.obfuscate(code, PROFILES[profile]).getObfuscatedCode();
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.writeFileSync(dest, result);
  console.log(`  ${path.relative(root, src)}  ${(code.length / 1024).toFixed(0)} KB -> ${(result.length / 1024).toFixed(0)} KB`);
}

function copy(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
}

const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
const isPublic = process.argv.includes('--public');
const serverUrl = isPublic ? String(pkg.babuTela?.publicServerUrl || '').replace(/\/+$/, '') : 'http://localhost:3210';
if (isPublic && !/^https:\/\/[a-z0-9.-]+\.[a-z]{2,}$/i.test(serverUrl)) {
  console.error('Defina "babuTela.publicServerUrl" no package.json com o endereço HTTPS do seu servidor, por exemplo "https://chat.seudominio.com.br".');
  process.exit(1);
}

fs.rmSync(out, { recursive: true, force: true });
console.log(isPublic ? `Build público: o app vai se conectar a ${serverUrl}` : 'Build local: o app hospeda o próprio servidor');
console.log('Ofuscando o código para o instalador:');

// Desktop app: main process and preload run in Node; renderer runs in the page.
for (const rel of walk(path.join(root, 'app'))) {
  const src = path.join(root, 'app', rel);
  const dest = path.join(out, 'app', rel);
  if (!rel.endsWith('.js')) copy(src, dest);
  else obfuscate(src, dest, rel.startsWith('renderer') ? 'browser' : 'node');
}

fs.writeFileSync(path.join(out, 'app', 'config.json'), JSON.stringify({ serverUrl, embeddedServer: !isPublic }, null, 2));

// Built-in server, only in the local build (the test file never ships).
fs.mkdirSync(path.join(out, 'server'), { recursive: true });
if (!isPublic) {
  for (const name of ['index.js', 'db.js']) {
    obfuscate(path.join(root, 'server', name), path.join(out, 'server', name), 'node');
  }
}

function walk(dir, base = '') {
  const files = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = path.join(base, entry.name);
    if (entry.isDirectory()) files.push(...walk(path.join(dir, entry.name), rel));
    else files.push(rel);
  }
  return files;
}
