"""Generator for the umwelt and herd marks.

The mark states what the library does: a crowd of entities, one observer at the
center, and the view radius that decides which of the crowd that observer sees.
Entities inside the radius are solid; entities outside it are ghosted.

Geometry is deterministic. A fixed-seed LCG places the crowd and a relaxation
pass pushes overlapping dots apart, so regenerating from unchanged source
produces identical output. No dot extends past BOUND, which keeps the
composition clear of the canvas edge at every size.

Two optical sizes. The full form carries the ghost crowd and is for 64px and
up. Below that the crowd stops resolving and only muddies the shape, so the
small form drops it and thickens the ring. rustdoc renders a crate logo at 48px
(35px on narrow layouts), so the docs logo uses the small form.

Every shape carries a class: `ink` for the crowd, `ring` for the view radius,
`obs` for the observer. Styling those classes recolors the mark without
touching the geometry.
"""
import math

C = 32.0            # center of the 64x64 viewBox
VIEW_R = 20.0       # view radius, the ring
STROKE = 4.0
BOUND = 31.4        # no dot extends past this radius from center
MIN_GAP = 0.9       # clear space between any two dots
CENTER_R = 4.4      # observer
CENTER_GAP = 2.2    # extra clearance the observer demands

INK_LIGHT = "#14181D"
INK_DARK = "#E6E9EC"
ACCENT = "#1FA8C4"


class Rnd:
    """LCG. Seeded explicitly so the layout is reproducible."""

    def __init__(self, seed):
        self.s = seed

    def next(self):
        self.s = (1103515245 * self.s + 12345) % (1 << 31)
        return self.s / (1 << 31)

    def sym(self, a):
        return (self.next() * 2 - 1) * a


def _circle(x, y, r, fill, cls, op=1.0):
    o = "" if op >= 0.999 else f' opacity="{op:.2f}"'
    return f'<circle class="{cls}" cx="{x:.2f}" cy="{y:.2f}" r="{r:.2f}" fill="{fill}"{o}/>'


def _wrap(inner, vb, size, tile, tile_radius, scale, title, style=""):
    if scale != 1.0:
        t = C - C * scale
        inner = f'<g transform="translate({t:.3f} {t:.3f}) scale({scale})">{inner}</g>'
    bg = ""
    if tile:
        rx = f' rx="{tile_radius}"' if tile_radius else ""
        bg = f'<rect class="tile" width="{vb}" height="{vb}"{rx} fill="{tile}"/>'
    dim = f' width="{size}" height="{size}"' if size else ""
    ttl = f"<title>{title}</title>" if title else ""
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {vb} {vb}"{dim} '
            f'role="img">{ttl}{style}{bg}{inner}</svg>')


def _relax(dots, min_gap=MIN_GAP, banded=True):
    inner_lim = VIEW_R - STROKE / 2 - 1.2
    outer_lim = VIEW_R + STROKE / 2 + 1.2
    for _ in range(60):
        for i, a in enumerate(dots):
            for b in dots[i + 1:]:
                dx, dy = b["x"] - a["x"], b["y"] - a["y"]
                d = math.hypot(dx, dy) or 1e-6
                need = a["r"] + b["r"] + min_gap
                if d < need:
                    push = (need - d) / 2
                    ux, uy = dx / d, dy / d
                    a["x"] -= ux * push
                    a["y"] -= uy * push
                    b["x"] += ux * push
                    b["y"] += uy * push
        for a in dots:
            dx, dy = a["x"] - C, a["y"] - C
            d = math.hypot(dx, dy) or 1e-6
            ux, uy = dx / d, dy / d
            if banded:
                if a["band"] == "in":
                    d = max(d, CENTER_R + a["r"] + CENTER_GAP)
                    d = min(d, inner_lim - a["r"])
                else:
                    d = max(d, outer_lim + a["r"])
            d = min(d, BOUND - a["r"])
            a["x"], a["y"] = C + d * ux, C + d * uy
    return dots


def _place(rnd, dist, n, r, rot, ajit, djit, rjit, band):
    out = []
    for k in range(n):
        a = math.radians(rot + k * 360.0 / n - 90 + rnd.sym(ajit))
        d = dist + rnd.sym(djit)
        out.append({"x": C + d * math.cos(a), "y": C + d * math.sin(a),
                    "r": r + rnd.sym(rjit), "band": band})
    return out


def geometry():
    """umwelt: 11 entities in view, 30 out of view."""
    rnd = Rnd(37)
    dots = []
    dots += _place(rnd, 28.6, 18, 1.60, 8, 8.0, 1.1, 0.30, "far")
    dots += _place(rnd, 24.9, 12, 2.05, 2, 8.0, 1.2, 0.32, "near")
    dots += _place(rnd, 14.2, 8, 2.50, 0, 6.0, 0.9, 0.30, "in")
    dots += _place(rnd, 8.4, 3, 2.10, 40, 10.0, 0.5, 0.25, "in")
    return _relax(dots)


def herd_geometry():
    """herd: one crowd, even density out to the edge, no ring and no gap."""
    rnd = Rnd(91)
    dots = []
    for dist, n, r, band in ((7.6, 4, 2.55, "in"), (14.8, 9, 2.40, "in"),
                             (21.8, 14, 2.10, "near"), (28.4, 18, 1.65, "far")):
        dots += _place(rnd, dist, n, r, 0, 9.0, 1.1, 0.28, band)
    return _relax(dots, min_gap=1.15, banded=False)


def render(ink, accent, ghost_near=0.32, ghost_far=0.20, vb=64, size=None,
           tile=None, tile_radius=None, scale=1.0, title=None, style=""):
    """umwelt, full form. For 64px and up."""
    op = {"in": 1.0, "near": ghost_near, "far": ghost_far}
    body = [_circle(d["x"], d["y"], d["r"], ink, "ink", op[d["band"]]) for d in geometry()]
    body.insert(30, f'<circle class="ring" cx="{C}" cy="{C}" r="{VIEW_R}" '
                    f'fill="none" stroke="{ink}" stroke-width="{STROKE}"/>')
    body.append(_circle(C, C, CENTER_R, accent, "obs"))
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)


def render_small(ink, accent, vb=64, size=None, tile=None, tile_radius=None,
                 scale=1.0, title=None, style=""):
    """umwelt, small form. For 48px and below, and for the rustdoc logo."""
    rnd = Rnd(37)
    body = [f'<circle class="ring" cx="{C}" cy="{C}" r="19" fill="none" '
            f'stroke="{ink}" stroke-width="5"/>']
    for k in range(6):
        a = math.radians(k * 60 - 90 + rnd.sym(4.0))
        d = 12.0 + rnd.sym(0.4)
        body.append(_circle(C + d * math.cos(a), C + d * math.sin(a), 2.95, ink, "ink"))
    body.append(_circle(C, C, 5.4, accent, "obs"))
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)


def render_herd(ink, accent, ghost_near=0.55, ghost_far=0.34, vb=64, size=None,
                tile=None, tile_radius=None, scale=1.0, title=None, style=""):
    """herd, full form. The crowd umwelt observes, with one member accented."""
    op = {"in": 1.0, "near": ghost_near, "far": ghost_far}
    dots = herd_geometry()
    lead = min(range(4), key=lambda i: abs(math.hypot(dots[i]["x"] - C, dots[i]["y"] - C) - 7.6))
    body = []
    for i, d in enumerate(dots):
        if i == lead:
            body.append(_circle(d["x"], d["y"], d["r"] * 1.35, accent, "obs"))
        else:
            body.append(_circle(d["x"], d["y"], d["r"], ink, "ink", op[d["band"]]))
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)


def render_herd_small(ink, accent, vb=64, size=None, tile=None, tile_radius=None,
                      scale=1.0, title=None, style=""):
    """herd, small form: seven of the crowd, one of them accented."""
    body = []
    for k in range(6):
        a = math.radians(k * 60 - 90)
        x, y = C + 13.0 * math.cos(a), C + 13.0 * math.sin(a)
        body.append(_circle(x, y, 4.6, accent, "obs") if k == 1
                    else _circle(x, y, 4.6, ink, "ink"))
    body.append(_circle(C, C, 4.6, ink, "ink"))
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)


def adaptive_style(ink=INK_LIGHT, ink_dark=INK_DARK, accent=ACCENT):
    """Follow the viewer's color scheme. For favicons, which sit on a tab bar
    whose color the page does not control."""
    return ("<style>"
            f".ink{{fill:{ink}}}.ring{{stroke:{ink};fill:none}}.obs{{fill:{accent}}}"
            f"@media (prefers-color-scheme:dark){{.ink{{fill:{ink_dark}}}"
            f".ring{{stroke:{ink_dark}}}}}"
            "</style>")


# ---------------------------------------------------------------------------
# Micro forms, 16px only. At 16 the entity dots of the small form land on
# roughly one pixel each and read as smudges, so the micro forms keep the ring
# and the observer and drop the rest.
# ---------------------------------------------------------------------------

def render_micro(ink, accent, vb=64, size=None, tile=None, tile_radius=None,
                 scale=1.0, title=None, style=""):
    """umwelt at 16px: view radius and observer."""
    body = [f'<circle class="ring" cx="{C}" cy="{C}" r="18.5" fill="none" '
            f'stroke="{ink}" stroke-width="7"/>',
            _circle(C, C, 7.6, accent, "obs")]
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)


def render_herd_micro(ink, accent, vb=64, size=None, tile=None, tile_radius=None,
                      scale=1.0, title=None, style=""):
    """herd at 16px: four of the crowd, one accented."""
    body = []
    for k, (dx, dy) in enumerate(((-11, -11), (11, -11), (-11, 11), (11, 11))):
        body.append(_circle(C + dx, C + dy, 8.0, accent if k == 1 else ink,
                            "obs" if k == 1 else "ink"))
    return _wrap("".join(body), vb, size, tile, tile_radius, scale, title, style)
