Reno app icon
=============

reno.svg   the source (1024 canvas, macOS icon grid: an 824 px rounded square, the brand accent
           #8b7cf6 from src/ui/theme.rs on a #060606 ground).
Reno.icns  generated from it and committed. `cargo xtask bundle` copies it into Reno.app; nothing
           is rasterised at bundle time.
render.swift  rasterises the SVG to a 1024 px PNG with a transparent ground.

Regenerate after editing reno.svg (macOS, from this directory):

  mkdir -p /tmp/reno-icon
  swift render.swift reno.svg /tmp/reno-icon/icon-1024.png
  rm -rf /tmp/reno-icon/Reno.iconset && mkdir /tmp/reno-icon/Reno.iconset
  for s in 16 32 128 256 512; do
    sips -z $s $s /tmp/reno-icon/icon-1024.png --out /tmp/reno-icon/Reno.iconset/icon_${s}x${s}.png
    sips -z $((s*2)) $((s*2)) /tmp/reno-icon/icon-1024.png --out /tmp/reno-icon/Reno.iconset/icon_${s}x${s}@2x.png
  done
  iconutil -c icns /tmp/reno-icon/Reno.iconset -o Reno.icns

(`qlmanage -t -s 1024 -o <dir> reno.svg` also rasterises the SVG, but it fills the corners with
white; render.swift keeps them transparent.)
