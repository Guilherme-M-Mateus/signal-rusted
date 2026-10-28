"""Generates assets/icon.svg (and PNGs via rsvg-convert): a message bubble inside a Rust-style cog.
Original artwork, deliberately unlike Signal's trademarked logo. Run: python3 scripts/make_icon.py"""
import math
import pathlib
import subprocess

TEETH, R_OUT, R_IN, C = 14, 122, 104, 128


def cog() -> str:
    pts = []
    for i in range(TEETH):
        a = 2 * math.pi * i / TEETH
        step = 2 * math.pi / TEETH
        # Trapezoid tooth: base wider than the tip, then a valley at the body radius.
        for frac, r in ((-0.30, R_IN), (-0.18, R_OUT), (0.18, R_OUT), (0.30, R_IN)):
            t = a + frac * step
            pts.append(f"{C + r * math.cos(t):.2f},{C + r * math.sin(t):.2f}")
    return "M" + " L".join(pts) + " Z"


SVG = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256">
  <defs>
    <linearGradient id="rust" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#FF7A2F"/>
      <stop offset="1" stop-color="#C83A0A"/>
    </linearGradient>
  </defs>
  <path d="{cog()}" fill="url(#rust)" stroke="url(#rust)" stroke-width="6" stroke-linejoin="round"/>
  <circle cx="{C}" cy="{C}" r="86" fill="#1E1F24"/>
  <!-- Rounded-rectangle bubble with its tail at the bottom right. -->
  <path d="M78 84 h100 a18 18 0 0 1 18 18 v46 a18 18 0 0 1 -18 18 h-14 l14 22 l-34 -22 h-66
           a18 18 0 0 1 -18 -18 v-46 a18 18 0 0 1 18 -18 z" fill="#FFFFFF"/>
  <circle cx="102" cy="125" r="9" fill="url(#rust)"/>
  <circle cx="128" cy="125" r="9" fill="url(#rust)"/>
  <circle cx="154" cy="125" r="9" fill="url(#rust)"/>
</svg>
"""

out = pathlib.Path(__file__).resolve().parent.parent / "assets"
out.mkdir(exist_ok=True)
(out / "icon.svg").write_text(SVG)
for size in (16, 32, 48, 64, 128, 256, 512):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), str(out / "icon.svg"), "-o", str(out / f"icon-{size}.png")], check=True)
print("wrote", out)
