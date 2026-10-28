#!/bin/sh
# Renders the README's screenshots and GIFs into docs/images/ from the test suite's fake data
# (no real account involved). Needs ImageMagick (`magick`).
set -eu
cd "$(dirname "$0")/.."
cargo test --lib readme_media -- --ignored --quiet
cargo test --lib link_screen_snapshot --quiet
src=snapshots/readme
out=docs/images
mkdir -p "$out"

png() { magick "$1" -resize 1100x -strip "$out/$2.png"; }
# gif NAME DELAY(1/100 s) FRAMES...
gif() {
    name=$1 delay=$2
    shift 2
    magick -delay "$delay" -loop 0 "$@" -resize 900x -dither None -colors 160 -layers Optimize "$out/$name.gif"
}

png "$src/hero-wgpu.png" hero
png "$src/timer-menu-wgpu.png" timer-menu
png snapshots/link-dark-wgpu.png link
gif themes 140 "$src"/theme-*-wgpu.png
gif rounding 160 "$src"/rounding-*-wgpu.png
gif fun-panel 160 "$src"/fun-*-wgpu.png
gif settings 180 "$src"/settings-*-wgpu.png
ls -la "$out"
