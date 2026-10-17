//go:build !obf

// Package frontend holds the app's interface (HTML, CSS, JavaScript). The
// desktop app shows it in its window and the server serves it as the web
// version. Builds with -tags obf embed the obfuscated copy in dist/ instead
// (see build/obfuscate.js).
package frontend

import "embed"

//go:embed index.html style.css app.js voice.js native.js config.js icon.png manifest.json
var Files embed.FS
