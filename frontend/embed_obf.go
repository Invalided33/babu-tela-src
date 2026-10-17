//go:build obf

package frontend

import (
	"embed"
	"io/fs"
)

//go:embed dist
var dist embed.FS

// Files is the obfuscated copy produced by build/obfuscate.js.
var Files, _ = fs.Sub(dist, "dist")
