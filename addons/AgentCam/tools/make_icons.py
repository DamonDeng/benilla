#!/usr/bin/env python3
"""Draw AgentCam's button icons as 64x64 32-bit TGAs (pure Python, no dependencies).

    python3 -I addons/AgentCam/tools/make_icons.py [--preview /tmp/agentcam_icons.png]

Each icon is a rounded tile in a per-function colour with a white glyph and a soft shadow,
4x4 supersampled. Writes addons/AgentCam/icons/<name>.tga; the addon references them as
Interface\\AddOns\\AgentCam\\icons\\<name>. New files need a client restart, edits a /reload.
"""
import math
import os
import struct
import sys
import zlib

SIZE = 64
SS = 4  # supersampling per axis
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "icons")

WHITE = (255, 255, 255)
SHADOW = (0, 0, 0)


# ----------------------------------------------------------------------------- shapes
# A shape is f(x, y) -> bool over the unit square, y pointing down.

def circle(cx, cy, r):
    return lambda x, y: (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def ellipse(cx, cy, rx, ry, angle=0.0):
    c, s = math.cos(angle), math.sin(angle)

    def f(x, y):
        dx, dy = x - cx, y - cy
        u, v = dx * c + dy * s, -dx * s + dy * c
        return (u / rx) ** 2 + (v / ry) ** 2 <= 1

    return f


def ring(cx, cy, r_out, r_in):
    return lambda x, y: r_in * r_in <= (x - cx) ** 2 + (y - cy) ** 2 <= r_out * r_out


def arc(cx, cy, r, width, a0, a1):
    """Ring band from angle a0 to a1 (degrees, 0 = right, clockwise because y is down)."""
    def f(x, y):
        d = math.hypot(x - cx, y - cy)
        if abs(d - r) > width / 2:
            return False
        a = math.degrees(math.atan2(y - cy, x - cx)) % 360
        lo, hi = a0 % 360, a1 % 360
        return lo <= a <= hi if lo <= hi else (a >= lo or a <= hi)

    return f


def seg(x0, y0, x1, y1, w):
    """Capsule: a line with round caps, width w."""
    def f(x, y):
        dx, dy = x1 - x0, y1 - y0
        t = max(0.0, min(1.0, ((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy)))
        return math.hypot(x - (x0 + t * dx), y - (y0 + t * dy)) <= w / 2

    return f


def poly(*pts):
    def f(x, y):
        inside = False
        n = len(pts)
        for i in range(n):
            xa, ya = pts[i]
            xb, yb = pts[(i + 1) % n]
            if (ya > y) != (yb > y) and x < (xb - xa) * (y - ya) / (yb - ya) + xa:
                inside = not inside
        return inside

    return f


def rrect(x0, y0, x1, y1, r):
    def f(x, y):
        qx = max(x0 + r - x, 0, x - (x1 - r))
        qy = max(y0 + r - y, 0, y - (y1 - r))
        return x0 <= x <= x1 and y0 <= y <= y1 and qx * qx + qy * qy <= r * r

    return f


def union(*fs):
    return lambda x, y: any(f(x, y) for f in fs)


def minus(a, *bs):
    return lambda x, y: a(x, y) and not any(b(x, y) for b in bs)


def shifted(f, dx, dy):
    return lambda x, y: f(x - dx, y - dy)


def arrowhead(tx, ty, angle_deg, size):
    """Triangle with its tip at (tx, ty) pointing along angle."""
    a = math.radians(angle_deg)
    bx, by = tx - math.cos(a) * size, ty - math.sin(a) * size
    px, py = -math.sin(a) * size * 0.62, math.cos(a) * size * 0.62
    return poly((tx, ty), (bx + px, by + py), (bx - px, by - py))


def person(cx, cy, s):
    """Head and shoulders."""
    return union(circle(cx, cy - 0.13 * s, 0.11 * s),
                 minus(ellipse(cx, cy + 0.17 * s, 0.22 * s, 0.17 * s),
                       lambda x, y: y > cy + 0.2 * s))


# ----------------------------------------------------------------------------- glyphs

def g_view():
    lens = lambda x, y: circle(0.5, 0.86, 0.5)(x, y) and circle(0.5, 0.14, 0.5)(x, y)
    return [(minus(lens, circle(0.5, 0.5, 0.16)), WHITE),
            (circle(0.5, 0.5, 0.16), (70, 150, 255)),
            (circle(0.5, 0.5, 0.075), (15, 20, 40)),
            (circle(0.535, 0.465, 0.028), WHITE)]


def g_follow():
    def foot(cx, cy, ang):
        # Sole (toe end up) and a separate heel below it, rotated by ang degrees.
        a = math.radians(ang)
        ux, uy = math.sin(a), -math.cos(a)  # "forward" unit vector
        sole = ellipse(cx + ux * 0.05, cy + uy * 0.05, 0.065, 0.1, a)
        heel = ellipse(cx - ux * 0.11, cy - uy * 0.11, 0.055, 0.05, a)
        return union(sole, heel)
    # A left and a right footprint stepping up-right toward a chevron: "go after it".
    feet = union(foot(0.3, 0.66, 25), foot(0.52, 0.5, 25))
    chevron = union(seg(0.66, 0.15, 0.83, 0.22, 0.075), seg(0.83, 0.22, 0.77, 0.39, 0.075))
    return [(feet, WHITE), (chevron, (255, 230, 120))]


def g_watch():
    body = union(circle(0.32, 0.58, 0.17), circle(0.68, 0.58, 0.17),
                 rrect(0.2, 0.3, 0.42, 0.55, 0.05), rrect(0.58, 0.3, 0.8, 0.55, 0.05),
                 rrect(0.42, 0.42, 0.58, 0.56, 0.02))
    return [(minus(body, circle(0.32, 0.58, 0.1), circle(0.68, 0.58, 0.1)), WHITE),
            (union(circle(0.32, 0.58, 0.1), circle(0.68, 0.58, 0.1)), (120, 210, 255))]


def g_me():
    return [(person(0.47, 0.5, 1.15), WHITE),
            (arc(0.5, 0.5, 0.36, 0.06, 200, 340), (255, 220, 110)),
            (arrowhead(0.83, 0.41, 75, 0.14), (255, 220, 110))]


def g_go():
    portal = union(ring(0.62, 0.5, 0.25, 0.19), ring(0.62, 0.5, 0.13, 0.08))
    arrow = union(seg(0.12, 0.5, 0.5, 0.5, 0.09), arrowhead(0.64, 0.5, 0, 0.2))
    return [(portal, (170, 220, 255)), (arrow, WHITE)]


def g_auto():
    a = union(arc(0.5, 0.5, 0.3, 0.075, 200, 340), arrowhead(0.79, 0.62, 100, 0.15),
              arc(0.5, 0.5, 0.3, 0.075, 20, 160), arrowhead(0.21, 0.38, 280, 0.15))
    play = poly((0.43, 0.38), (0.43, 0.62), (0.62, 0.5))
    return [(a, WHITE), (play, (255, 235, 140))]


def g_cam():
    corners = []
    for sx, sy in ((-1, -1), (1, -1), (-1, 1), (1, 1)):
        tx, ty = 0.5 + sx * 0.33, 0.5 + sy * 0.33
        corners += [seg(tx, ty, tx - sx * 0.17, ty, 0.07), seg(tx, ty, tx, ty - sy * 0.17, 0.07)]
    mountain = poly((0.3, 0.64), (0.44, 0.42), (0.53, 0.54), (0.6, 0.46), (0.71, 0.64))
    return [(union(*corners), WHITE), (mountain, (190, 240, 255)), (circle(0.63, 0.36, 0.045), (255, 230, 120))]


def g_invis():
    # A dashed outline of a person: present but unseen.
    hx, hy, hr = 0.5, 0.33, 0.14
    head = ring(hx, hy, hr, hr - 0.055)
    body = minus(ellipse(0.5, 0.75, 0.3, 0.25), ellipse(0.5, 0.77, 0.24, 0.19), lambda x, y: y > 0.8)
    body_base = lambda x, y: 0.745 <= y <= 0.8 and 0.2 <= x <= 0.8 and not (0.26 < x < 0.74 and y < 0.8 - 0.0)
    def dashed(f, cx, cy, step):
        return lambda x, y: f(x, y) and (math.degrees(math.atan2(y - cy, x - cx)) % step) < step * 0.58
    return [(union(dashed(head, hx, hy, 40), dashed(body, 0.5, 0.75, 24)), WHITE),
            (seg(0.24, 0.8, 0.76, 0.8, 0.055), (200, 215, 240, 160))]


def g_shot():
    body = union(rrect(0.16, 0.34, 0.84, 0.76, 0.07), rrect(0.36, 0.25, 0.58, 0.38, 0.03))
    return [(minus(body, circle(0.5, 0.55, 0.15)), WHITE),
            (ring(0.5, 0.55, 0.1, 0.0), (90, 120, 160)),
            (circle(0.46, 0.51, 0.03), WHITE),
            (circle(0.74, 0.42, 0.03), (255, 210, 90))]


def g_dismount():
    shoe = minus(arc(0.5, 0.42, 0.22, 0.11, 150, 390), lambda x, y: False)
    nails = union(*[circle(0.5 + math.cos(math.radians(a)) * 0.22, 0.42 + math.sin(math.radians(a)) * 0.22, 0.018)
                    for a in (170, 215, 250, 290, 325, 370)])
    arrow = union(seg(0.5, 0.5, 0.5, 0.72, 0.08), arrowhead(0.5, 0.88, 90, 0.17))
    return [(minus(shoe, nails), WHITE), (arrow, (255, 220, 110))]


def g_gm():
    # A crown: GM mode.
    crown = poly((0.18, 0.72), (0.18, 0.34), (0.35, 0.52), (0.5, 0.26), (0.65, 0.52), (0.82, 0.34), (0.82, 0.72))
    band = rrect(0.16, 0.7, 0.84, 0.8, 0.02)
    jewels = union(circle(0.18, 0.31, 0.045), circle(0.5, 0.22, 0.05), circle(0.82, 0.31, 0.045))
    return [(union(crown, band), (255, 215, 90)), (jewels, WHITE),
            (union(circle(0.35, 0.62, 0.035), circle(0.5, 0.62, 0.035), circle(0.65, 0.62, 0.035)), (200, 60, 60))]


def g_settings():
    teeth = [poly(*[(0.5 + math.cos(a + d) * r, 0.5 + math.sin(a + d) * r)
                    for d, r in ((-0.2, 0.25), (-0.13, 0.37), (0.13, 0.37), (0.2, 0.25))])
             for a in [i * math.pi / 4 for i in range(8)]]
    gear = minus(union(circle(0.5, 0.5, 0.27), *teeth), circle(0.5, 0.5, 0.11))
    return [(gear, WHITE)]


def g_obs():
    # Stat bars (health, power, experience): the observe panel.
    return [(rrect(0.18, 0.24, 0.82, 0.36, 0.03), (90, 220, 90)),
            (rrect(0.18, 0.44, 0.66, 0.56, 0.03), (110, 150, 255)),
            (rrect(0.18, 0.64, 0.5, 0.76, 0.03), (200, 120, 255))]


def g_team():
    # Three heads and shoulders: a group.
    def person(cx, cy, s):
        return union(circle(cx, cy - 0.12 * s, 0.09 * s),
                     rrect(cx - 0.15 * s, cy, cx + 0.15 * s, cy + 0.2 * s, 0.07 * s))
    return [(union(person(0.28, 0.52, 0.8), person(0.72, 0.52, 0.8)), (170, 200, 230)),
            (person(0.5, 0.48, 1.0), WHITE)]


# name: (glyph, tile top colour, tile bottom colour)
ICONS = {
    "view": (g_view, (130, 80, 200), (55, 25, 100)),
    "follow": (g_follow, (60, 170, 90), (20, 80, 40)),
    "watch": (g_watch, (40, 150, 160), (10, 65, 75)),
    "me": (g_me, (200, 140, 50), (100, 60, 15)),
    "go": (g_go, (60, 110, 220), (20, 40, 110)),
    "auto": (g_auto, (230, 120, 40), (120, 50, 10)),
    "cam": (g_cam, (40, 160, 210), (10, 70, 110)),
    "invis": (g_invis, (90, 100, 130), (35, 40, 60)),
    "shot": (g_shot, (210, 60, 70), (100, 20, 30)),
    "dismount": (g_dismount, (150, 95, 55), (70, 40, 20)),
    "gm": (g_gm, (150, 40, 50), (70, 15, 25)),
    "settings": (g_settings, (120, 125, 135), (50, 52, 58)),
    "obs": (g_obs, (70, 80, 95), (25, 28, 35)),
    "team": (g_team, (60, 140, 120), (20, 65, 55)),
}


# ----------------------------------------------------------------------------- raster

def blend(dst, color, alpha):
    a = alpha * (color[3] / 255 if len(color) > 3 else 1.0)
    r, g, b, da = dst
    out_a = a + da * (1 - a)
    if out_a == 0:
        return (0.0, 0.0, 0.0, 0.0)
    mix = lambda c, d: (c * a + d * da * (1 - a)) / out_a
    return (mix(color[0], r), mix(color[1], g), mix(color[2], b), out_a)


def coverage(f, px, py):
    hits = 0
    for sy in range(SS):
        for sx in range(SS):
            if f((px + (sx + 0.5) / SS) / SIZE, (py + (sy + 0.5) / SS) / SIZE):
                hits += 1
    return hits / (SS * SS)


def render(name):
    glyph_fn, top, bottom = ICONS[name]
    layers = glyph_fn()
    tile = rrect(0.0, 0.0, 1.0, 1.0, 0.16)
    inner = rrect(0.045, 0.045, 0.955, 0.955, 0.12)
    shine = lambda x, y: inner(x, y) and y < 0.45 and ellipse(0.5, 0.0, 0.75, 0.45)(x, y)
    glyph_union = union(*[f for f, _ in layers])
    shadow = shifted(glyph_union, 0.025, 0.035)
    pixels = []
    for py in range(SIZE):
        t = py / (SIZE - 1)
        grad = tuple(top[i] * (1 - t) + bottom[i] * t for i in range(3))
        edge = tuple(min(255, c * 1.5 + 30) for c in top)
        for px in range(SIZE):
            c = (0.0, 0.0, 0.0, 0.0)
            c = blend(c, (10, 10, 12), coverage(tile, px, py))                 # dark rim
            c = blend(c, edge, coverage(inner, px, py))                         # bevel
            c = blend(c, grad, coverage(rrect(0.075, 0.075, 0.925, 0.925, 0.1), px, py))
            c = blend(c, (255, 255, 255, 40), coverage(shine, px, py))          # top gloss
            c = blend(c, (0, 0, 0, 120), coverage(shadow, px, py))
            for f, col in layers:
                cov = coverage(f, px, py)
                if cov:
                    c = blend(c, col, cov)
            pixels.append(tuple(int(round(v)) for v in c[:3]) + (int(round(c[3] * 255)),))
    return pixels


def write_tga(path, pixels):
    # Type 2 truecolour, 32 bpp, 8 alpha bits, top-left origin (descriptor 0x28); BGRA rows.
    header = struct.pack("<BBBHHBHHHHBB", 0, 0, 2, 0, 0, 0, 0, 0, SIZE, SIZE, 32, 0x28)
    body = bytearray()
    for r, g, b, a in pixels:
        body += bytes((b, g, r, a))
    with open(path, "wb") as fh:
        fh.write(header + bytes(body))


def write_png_sheet(path, images, scale=2, pad=8):
    n = len(images)
    w = n * (SIZE * scale + pad) + pad
    h = SIZE * scale + 2 * pad
    rows = []
    for y in range(h):
        row = bytearray([0])
        for x in range(w):
            col = (40, 40, 44, 255)
            i, ox = divmod(x - pad, SIZE * scale + pad)
            oy = y - pad
            if 0 <= i < n and 0 <= ox < SIZE * scale and 0 <= oy < SIZE * scale:
                p = images[i][(oy // scale) * SIZE + ox // scale]
                a = p[3] / 255
                col = tuple(int(p[k] * a + col[k] * (1 - a)) for k in range(3)) + (255,)
            row += bytes(col)
        rows.append(bytes(row))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(png)


def main():
    os.makedirs(OUT, exist_ok=True)
    images = []
    for name in ICONS:
        px = render(name)
        write_tga(os.path.join(OUT, name + ".tga"), px)
        images.append(px)
        print("wrote", os.path.normpath(os.path.join(OUT, name + ".tga")))
    if "--preview" in sys.argv:
        path = sys.argv[sys.argv.index("--preview") + 1]
        write_png_sheet(path, images)
        print("preview", path, "order:", " ".join(ICONS))


if __name__ == "__main__":
    main()
