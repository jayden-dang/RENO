Reno app icon
=============

The mark is an R drawn the way the Klynt mark draws its K: solid pieces, edges only at 0, 45 and
90 degrees, one even gutter (about 6.7% of the box), no stroke, no gradient. It keeps the K's
split stem (two triangles) and turns the K's upper arm into the R's bowl; the lower arm stays the
leg. The in-app copy is assets/icons/mark.svg (16 grid, currentColor). Locked in
docs/design/2026-11-19-logo-ui-brief.md.

reno.svg        the source (1024 canvas, macOS icon grid: an 824 px rounded square). Ink plate
                #1d1c1a, paper mark #ecebe6, the mark 430 px tall.
reno-small.svg  the same drawing with the mark 540 px tall, for the 16 and 32 px slots only: at
                430 px the gutters close up below 64 px.
Reno.icns       generated from both and committed. `cargo xtask bundle` copies it into Reno.app;
                nothing is rasterised at bundle time.
render.swift    rasterises an SVG to a 1024 px PNG with a transparent ground.

Regenerate after editing either SVG (macOS, from this directory):

  T=$(mktemp -d)
  swift render.swift reno.svg $T/big.png
  swift render.swift reno-small.svg $T/small.png
  mkdir $T/Reno.iconset
  for s in 16 32 128 256 512; do
    for x in 1 2; do
      px=$((s*x)); src=$T/big.png; [ $px -le 32 ] && src=$T/small.png
      name=icon_${s}x${s}; [ $x = 2 ] && name=${name}@2x
      sips -z $px $px $src --out $T/Reno.iconset/$name.png
    done
  done
  iconutil -c icns $T/Reno.iconset -o Reno.icns

(`qlmanage -t -s 1024 -o <dir> reno.svg` also rasterises the SVG, but it fills the corners with
white; render.swift keeps them transparent.)
