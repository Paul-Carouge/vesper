#!/usr/bin/env python3
"""Generates Vesper's icons: the app icon (scalable SVG, pixel-tuned SVGs for
16/22/24 px, PNGs for every hicolor size) and the symbolic tray icons.

The mark: an evening star ("vesper") rising on the rim of a lens, around a
record dot. Run from anywhere; needs `rsvg-convert` (librsvg) for the PNGs.
Writes `crates/vesper-app/data/icons/hicolor/`, laid out as installed.
"""

import math
import os
import subprocess

APP_ID = "io.github.PaulCarouge.Vesper"
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
OUT = os.path.join(ROOT, "crates/vesper-app/data/icons/hicolor")
PNG_SIZES = (16, 22, 24, 32, 48, 64, 128, 256, 512)
# Up to 32 px the master blurs: these sizes have their own pixel-snapped geometry.
TUNED = (16, 22, 24, 32)

# Brand: one violet, fading to the recording coral.
VIOLET = "#A99CFF"
MID = "#C47DD6"
CORAL = "#FF5C52"
STAR = "#FFFFFF"


def n(v):
    """Short decimal: 3 places at most, no trailing zeros."""
    s = f"{v:.3f}".rstrip("0").rstrip(".")
    return "0" if s == "-0" else s


def polar(cx, cy, r, deg):
    a = math.radians(deg)
    return cx + r * math.cos(a), cy + r * math.sin(a)


def star(cx, cy, r, k):
    """Four-pointed star, its arms `r` long, pinched to `k` near the center."""
    return (
        f"M{n(cx)} {n(cy - r)}Q{n(cx + k)} {n(cy - k)} {n(cx + r)} {n(cy)}"
        f"Q{n(cx + k)} {n(cy + k)} {n(cx)} {n(cy + r)}"
        f"Q{n(cx - k)} {n(cy + k)} {n(cx - r)} {n(cy)}"
        f"Q{n(cx - k)} {n(cy - k)} {n(cx)} {n(cy - r)}Z"
    )


def circle(cx, cy, r, ccw=False):
    """A full circle as a path (two half arcs)."""
    sweep = 0 if ccw else 1
    return (
        f"M{n(cx - r)} {n(cy)}A{n(r)} {n(r)} 0 1 {sweep} {n(cx + r)} {n(cy)}"
        f"A{n(r)} {n(r)} 0 1 {sweep} {n(cx - r)} {n(cy)}Z"
    )


def band(cx, cy, r, w, a1, a2, caps=True):
    """A stroked arc from `a1` to `a2` (degrees, clockwise) as a filled
    outline: symbolic icons are fills only. Round caps unless `caps` is off."""
    ro, ri = r + w / 2, r - w / 2
    large = 1 if (a2 - a1) % 360 > 180 else 0
    o1, o2 = polar(cx, cy, ro, a1), polar(cx, cy, ro, a2)
    i1, i2 = polar(cx, cy, ri, a1), polar(cx, cy, ri, a2)
    cap = f"A{n(w / 2)} {n(w / 2)} 0 0 1" if caps else "L"
    return (
        f"M{n(o1[0])} {n(o1[1])}A{n(ro)} {n(ro)} 0 {large} 1 {n(o2[0])} {n(o2[1])}"
        f"{cap} {n(i2[0])} {n(i2[1])}"
        f"A{n(ri)} {n(ri)} 0 {large} 0 {n(i1[0])} {n(i1[1])}"
        f"{cap} {n(o1[0])} {n(o1[1])}Z"
    )


def arc(cx, cy, r, a1, a2):
    """An open arc path, for strokes."""
    large = 1 if (a2 - a1) % 360 > 180 else 0
    x1, y1 = polar(cx, cy, r, a1)
    x2, y2 = polar(cx, cy, r, a2)
    return f"M{n(x1)} {n(y1)}A{n(r)} {n(r)} 0 {large} 1 {n(x2)} {n(y2)}"


# ---- App icon ---------------------------------------------------------------


def app_icon(size, g):
    """The full-color app icon on a `size` grid. `g` holds the geometry."""
    s = size
    t0, t1, rx = g["tile"]
    tw = t1 - t0
    cx = cy = g["center"]
    R, sw, gap, ang = g["ring"], g["ring_w"], g["gap"], g["angle"]
    sx, sy = g.get("star_at") or polar(cx, cy, R, ang)
    sr, sk = g["star"]
    dot = g["dot"]
    # Gradients in user space, so ring and dot share one light direction.
    ga, gb = cx - R, cx + R
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{s}" height="{s}" viewBox="0 0 {s} {s}">',
        "<defs>",
        f'<linearGradient id="t" x1="0" y1="{n(t0)}" x2="0" y2="{n(t1)}" gradientUnits="userSpaceOnUse">'
        '<stop offset="0" stop-color="#2B2C37"/><stop offset="1" stop-color="#0F1015"/></linearGradient>',
        f'<linearGradient id="e" x1="0" y1="{n(t0)}" x2="0" y2="{n(t1)}" gradientUnits="userSpaceOnUse">'
        '<stop offset="0" stop-color="#FFF" stop-opacity=".16"/>'
        '<stop offset=".4" stop-color="#FFF" stop-opacity=".04"/>'
        '<stop offset="1" stop-color="#FFF" stop-opacity=".02"/></linearGradient>',
        f'<linearGradient id="g" x1="{n(ga)}" y1="{n(ga)}" x2="{n(gb)}" y2="{n(gb)}" gradientUnits="userSpaceOnUse">'
        f'<stop offset="0" stop-color="{VIOLET}"/><stop offset=".55" stop-color="{MID}"/>'
        f'<stop offset="1" stop-color="{CORAL}"/></linearGradient>',
    ]
    if g.get("glow"):
        parts.append(
            f'<radialGradient id="h" cx="{n(sx)}" cy="{n(sy)}" r="{n(g["glow"])}" gradientUnits="userSpaceOnUse">'
            '<stop offset="0" stop-color="#E9E4FF" stop-opacity=".5"/>'
            '<stop offset=".35" stop-color="#B9ADFF" stop-opacity=".16"/>'
            '<stop offset="1" stop-color="#9A8CFF" stop-opacity="0"/></radialGradient>'
        )
    if g.get("shine"):
        parts.append(
            f'<radialGradient id="s" cx="{n(cx - dot * .45)}" cy="{n(cy - dot * .5)}" r="{n(dot * 1.3)}" gradientUnits="userSpaceOnUse">'
            '<stop offset="0" stop-color="#FFF" stop-opacity=".38"/>'
            '<stop offset="1" stop-color="#FFF" stop-opacity="0"/></radialGradient>'
        )
    parts.append("</defs>")
    # Drop shadow: stacked translucent tiles (no filters, Qt SVG has none).
    for dy, op in g.get("shadow", ()):
        parts.append(
            f'<rect x="{n(t0)}" y="{n(t0 + dy)}" width="{n(tw)}" height="{n(tw)}" rx="{n(rx)}" fill="#000" fill-opacity="{op}"/>'
        )
    parts.append(f'<rect x="{n(t0)}" y="{n(t0)}" width="{n(tw)}" height="{n(tw)}" rx="{n(rx)}" fill="url(#t)"/>')
    if g.get("edge"):
        e = g["edge"] / 2
        parts.append(
            f'<rect x="{n(t0 + e)}" y="{n(t0 + e)}" width="{n(tw - 2 * e)}" height="{n(tw - 2 * e)}" rx="{n(rx - e)}" '
            f'fill="none" stroke="url(#e)" stroke-width="{n(g["edge"])}"/>'
        )
    if g.get("glow"):
        parts.append(f'<circle cx="{n(sx)}" cy="{n(sy)}" r="{n(g["glow"])}" fill="url(#h)"/>')
    parts.append(
        f'<path d="{arc(cx, cy, R, ang + gap, ang + 360 - gap)}" fill="none" stroke="url(#g)" '
        f'stroke-width="{n(sw)}" stroke-linecap="round"/>'
    )
    parts.append(f'<circle cx="{n(cx)}" cy="{n(cy)}" r="{n(dot)}" fill="url(#g)"/>')
    if g.get("shine"):
        parts.append(f'<circle cx="{n(cx)}" cy="{n(cy)}" r="{n(dot)}" fill="url(#s)"/>')
    if g.get("star_px"):
        # Pixel star: the core pixel, then arms fading out, one value per step.
        x0, y0 = g["star_px"][0]
        for d, op in enumerate(g["star_px"][1]):
            cells = [(0, 0)] if d == 0 else [(d, 0), (-d, 0), (0, d), (0, -d)]
            for dx, dy in cells:
                o = "" if op == 1 else f' fill-opacity="{op}"'
                parts.append(f'<rect x="{x0 + dx}" y="{y0 + dy}" width="1" height="1" fill="{STAR}"{o}/>')
    else:
        parts.append(f'<path d="{star(sx, sy, sr, sk)}" fill="{STAR}"/>')
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


MASTER = {
    "tile": (8, 120, 28),
    "shadow": ((1, ".10"), (2, ".08"), (4, ".05")),
    "edge": 1.5,
    "center": 64,
    "ring": 34,
    "ring_w": 8,
    "gap": 27,
    "angle": -45,
    "dot": 17,
    "star": (18, 2.6),
    "glow": 20,
    "shine": True,
}

SMALL = {
    16: {
        "tile": (1, 15, 3.5),
        "center": 8,
        "ring": 4.5,
        "ring_w": 1.5,
        "gap": 34,
        "angle": -45,
        "dot": 2,
        "star": (0, 0),
        "star_at": (11.5, 4.5),
        "star_px": ((11, 4), (1, .8, .3)),
    },
    22: {
        "tile": (1, 21, 5),
        "shadow": ((1, ".12"),),
        "edge": 1,
        "center": 11,
        "ring": 6.25,
        "ring_w": 1.5,
        "gap": 30,
        "angle": -45,
        "dot": 3,
        "star": (0, 0),
        "star_at": (15.5, 6.5),
        "star_px": ((15, 6), (1, .85, .45, .15)),
        "glow": 3.5,
    },
    32: {
        "tile": (2, 30, 7),
        "shadow": ((1, ".12"), (2, ".06")),
        "edge": 1,
        "center": 16,
        "ring": 9,
        "ring_w": 2.25,
        "gap": 28,
        "angle": -45,
        "dot": 4.5,
        "star": (0, 0),
        "star_at": (22.5, 9.5),
        "star_px": ((22, 9), (1, .95, .65, .35, .12)),
        "glow": 5,
        "shine": True,
    },
    24: {
        "tile": (1, 23, 5.5),
        "shadow": ((1, ".12"),),
        "edge": 1,
        "center": 12,
        "ring": 6.75,
        "ring_w": 1.75,
        "gap": 30,
        "angle": -45,
        "dot": 3,
        "star": (0, 0),
        "star_at": (16.5, 7.5),
        "star_px": ((16, 7), (1, .9, .5, .2)),
        "glow": 4,
    },
}


# ---- Symbolic icons ---------------------------------------------------------

SYM_HEAD = (
    '<svg xmlns="http://www.w3.org/2000/svg" width="22" height="22" viewBox="0 0 22 22">\n'
    '<style type="text/css" id="current-color-scheme">'
    ".ColorScheme-Text{color:#232629;}.ColorScheme-NegativeText{color:#da4453;}</style>\n"
)
TEXT = 'class="ColorScheme-Text" fill="currentColor"'
RED = 'class="ColorScheme-NegativeText" fill="currentColor"'

# 22 px grid, centered on (11, 11).
C = 11
RING_R, RING_W = 7.25, 1.5
# On the ring at -45 degrees, snapped to a pixel center so the arms stay crisp.
STAR_AT = (16.5, 5.5)


def sym(*paths):
    body = "".join(f'<path d="{d}" {attrs}/>\n' for d, attrs in paths)
    return SYM_HEAD + body + "</svg>\n"


def sym_mark(dot_r, dot_attrs):
    ring = band(C, C, RING_R, RING_W, -45 + 32, -45 + 360 - 32)
    return sym(
        (ring, TEXT),
        (star(STAR_AT[0], STAR_AT[1], 3.5, 0.55), TEXT),
        (circle(C, C, dot_r), dot_attrs),
    )


def sym_replay():
    # The lens turns into a loop running backwards: an arrowhead on top,
    # pointing counterclockwise.
    a1, a2 = -90 + 20, -90 + 360 - 40
    ring = band(C, C, RING_R, RING_W, a1, a2)
    tip = polar(C, C, RING_R, a1 - 26)
    outer = polar(C, C, RING_R + 3.5, a1 + 1)
    inner = polar(C, C, RING_R - 3.5, a1 + 1)
    head = f"M{n(tip[0])} {n(tip[1])}L{n(outer[0])} {n(outer[1])}L{n(inner[0])} {n(inner[1])}Z"
    return sym((ring, TEXT), (head, TEXT), (circle(C, C, 3), TEXT))


def sym_live():
    # On air: the dot sends waves both ways.
    waves = "".join(
        band(C, C, r, 1.5, a - 38, a + 38) for r in (5.25, 8.75) for a in (0, 180)
    )
    return sym((waves, TEXT), (circle(C, C, 2.75), RED))


SYMBOLIC = {
    f"scalable/apps/{APP_ID}-symbolic.svg": lambda: sym_mark(3, TEXT),
    f"scalable/status/{APP_ID}-replay-symbolic.svg": sym_replay,
    f"scalable/status/{APP_ID}-recording-symbolic.svg": lambda: sym_mark(3.5, RED),
    f"scalable/status/{APP_ID}-live-symbolic.svg": sym_live,
}


def write(rel, text):
    path = os.path.join(OUT, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    return path


def main():
    master = write(f"scalable/apps/{APP_ID}.svg", app_icon(128, MASTER))
    sources = {s: write(f"../src/{APP_ID}-{s}.svg", app_icon(s, SMALL[s])) for s in TUNED}
    for s in PNG_SIZES:
        src = sources.get(s, master)
        png = os.path.join(OUT, f"{s}x{s}/apps/{APP_ID}.png")
        os.makedirs(os.path.dirname(png), exist_ok=True)
        subprocess.run(["rsvg-convert", "-w", str(s), "-h", str(s), src, "-o", png], check=True)
    for rel, make in SYMBOLIC.items():
        write(rel, make())


if __name__ == "__main__":
    main()
