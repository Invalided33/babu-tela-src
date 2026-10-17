package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"math"
	"runtime"
	"sync"

	"github.com/energye/systray"

	"babutela/internal/buildinfo"
)

// The tray icon is the app icon scaled down; with unread mentions or direct
// messages it gets a red dot, like the Electron version had.

var (
	trayMu      sync.Mutex
	trayReady   bool
	trayIcons   = map[bool][]byte{}
	trayStarted bool
)

func startTray(a *App) {
	trayMu.Lock()
	if trayStarted {
		trayMu.Unlock()
		return
	}
	trayStarted = true
	trayMu.Unlock()
	trayIcons[false] = icoFromPNG(scaledIcon(32, false))
	trayIcons[true] = icoFromPNG(scaledIcon(32, true))
	go func() {
		// The tray has its own Windows message loop on this thread.
		runtime.LockOSThread()
		systray.Run(func() {
			systray.SetIcon(trayIcons[false])
			systray.SetTitle(buildinfo.Name)
			systray.SetTooltip(buildinfo.Name)
			systray.SetOnClick(func(systray.IMenu) { a.showWindow() })
			systray.SetOnDClick(func(systray.IMenu) { a.showWindow() })
			systray.SetOnRClick(func(menu systray.IMenu) { menu.ShowMenu() })
			systray.AddMenuItem("Abrir "+buildinfo.Name, "").Click(a.showWindow)
			systray.AddSeparator()
			systray.AddMenuItem("Ligar/desligar microfone", "").Click(func() { a.emit("tray:action", "mute") })
			systray.AddMenuItem("Ligar/desligar áudio", "").Click(func() { a.emit("tray:action", "deafen") })
			systray.AddSeparator()
			systray.AddMenuItem("Sair", "").Click(a.quit)
			trayMu.Lock()
			trayReady = true
			trayMu.Unlock()
		}, nil)
	}()
}

func stopTray() {
	trayMu.Lock()
	ready := trayReady
	trayReady = false
	trayMu.Unlock()
	if ready {
		systray.Quit()
	}
}

func setTrayBadge(n int) {
	trayMu.Lock()
	defer trayMu.Unlock()
	if !trayReady {
		return
	}
	systray.SetIcon(trayIcons[n > 0])
	if n == 0 {
		systray.SetTooltip(buildinfo.Name)
	} else if n == 1 {
		systray.SetTooltip(buildinfo.Name + " — 1 mensagem nova")
	} else {
		systray.SetTooltip(fmt.Sprintf("%s — %d mensagens novas", buildinfo.Name, n))
	}
}

// scaledIcon box-filters the 512px app icon down to size and optionally draws
// the unread dot (with a dark ring so it reads against the icon).
func scaledIcon(size int, badge bool) []byte {
	src, err := png.Decode(bytes.NewReader(appIcon))
	if err != nil {
		return appIcon
	}
	b := src.Bounds()
	dst := image.NewNRGBA(image.Rect(0, 0, size, size))
	for y := 0; y < size; y++ {
		for x := 0; x < size; x++ {
			x0, x1 := b.Min.X+x*b.Dx()/size, b.Min.X+(x+1)*b.Dx()/size
			y0, y1 := b.Min.Y+y*b.Dy()/size, b.Min.Y+(y+1)*b.Dy()/size
			var r, g, bl, al, n float64
			for sy := y0; sy < y1; sy++ {
				for sx := x0; sx < x1; sx++ {
					c := color.NRGBAModel.Convert(src.At(sx, sy)).(color.NRGBA)
					a := float64(c.A)
					r += float64(c.R) * a
					g += float64(c.G) * a
					bl += float64(c.B) * a
					al += a
					n++
				}
			}
			if al > 0 {
				dst.SetNRGBA(x, y, color.NRGBA{uint8(r / al), uint8(g / al), uint8(bl / al), uint8(al / n)})
			}
		}
	}
	if badge {
		cx, cy, rad := float64(size)*0.76, float64(size)*0.76, float64(size)*0.24
		for y := 0; y < size; y++ {
			for x := 0; x < size; x++ {
				d := math.Hypot(float64(x)+0.5-cx, float64(y)+0.5-cy)
				blend(dst, x, y, color.NRGBA{0x0d, 0x0c, 0x1d, 255}, clamp(rad+float64(size)*0.06-d+0.5))
				blend(dst, x, y, color.NRGBA{0xf2, 0x5f, 0x7a, 255}, clamp(rad-d+0.5))
			}
		}
	}
	var out bytes.Buffer
	png.Encode(&out, dst)
	return out.Bytes()
}

func clamp(v float64) float64 { return math.Max(0, math.Min(1, v)) }

func blend(img *image.NRGBA, x, y int, c color.NRGBA, k float64) {
	if k <= 0 {
		return
	}
	o := img.NRGBAAt(x, y)
	mix := func(a, b uint8) uint8 { return uint8(float64(a)*k + float64(b)*(1-k)) }
	img.SetNRGBA(x, y, color.NRGBA{mix(c.R, o.R), mix(c.G, o.G), mix(c.B, o.B), uint8(math.Max(float64(o.A), k*255))})
}

// icoFromPNG wraps a PNG in an .ico container (Windows Vista+ reads PNG
// entries), which is what the tray API expects.
func icoFromPNG(p []byte) []byte {
	img, err := png.DecodeConfig(bytes.NewReader(p))
	if err != nil {
		return p
	}
	var buf bytes.Buffer
	binary.Write(&buf, binary.LittleEndian, []uint16{0, 1, 1})
	dim := func(n int) uint8 {
		if n >= 256 {
			return 0
		}
		return uint8(n)
	}
	buf.Write([]byte{dim(img.Width), dim(img.Height), 0, 0})
	binary.Write(&buf, binary.LittleEndian, []uint16{1, 32})
	binary.Write(&buf, binary.LittleEndian, []uint32{uint32(len(p)), 22})
	buf.Write(p)
	return buf.Bytes()
}

func (a *App) emit(name string, data any) {
	if a.ctx != nil {
		emitEvent(a.ctx, name, data)
	}
}
