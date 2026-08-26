#!/usr/bin/env python3
"""Write every logo asset from umwelt_mark.py.

    python3 generate.py                 umwelt assets, plus herd's if the
                                        sibling checkout is present
    python3 generate.py --herd PATH     write herd's assets to PATH

Rasterizing needs Chrome, which is the only local tool that renders SVG to an
exact pixel size without a new dependency. Set CHROME to override the path.
SVG output does not need it.
"""
import argparse, os, shutil, struct, subprocess, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import umwelt_mark as M

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_HERD = os.path.normpath(os.path.join(HERE, "..", "..", "..", "herd", "assets", "logo"))
CHROME = os.environ.get(
    "CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")

INK, LIGHT, ACCENT = M.INK_LIGHT, M.INK_DARK, M.ACCENT
TILE_RX = 12         # 18.75% of the 64 unit viewBox
TILE_SCALE = 0.82    # inset of the full form within a tile
SMALL_SCALE = 0.90
MICRO_SCALE = 0.95
SMALL_CUT = 64       # below this pixel size, the small form is used
MICRO_CUT = 20       # below this, the micro form

ICON_SIZES = (16, 32, 48, 64, 128, 180, 256, 512, 1024)
MARK_SIZES = (256, 512, 1024)
ICO_SIZES = (16, 32, 48)

SOCIAL = """<html><head><meta charset="utf-8"><style>
html,body{{margin:0;padding:0;width:1280px;height:640px;background:#0F1318;overflow:hidden}}
.wrap{{display:flex;align-items:center;height:640px;padding:0 96px;box-sizing:border-box;gap:64px}}
img{{width:260px;height:260px;flex:none}}
h1{{font:600 108px/1 -apple-system,'SF Pro Display','Helvetica Neue',sans-serif;
letter-spacing:-.035em;color:#E6E9EC;margin:0}}
p{{font:400 34px/1.35 -apple-system,'Helvetica Neue',sans-serif;color:#8B949E;
margin:22px 0 0;max-width:660px}}
.rule{{width:64px;height:5px;background:#1FA8C4;border-radius:3px;margin:30px 0 0}}
</style></head><body><div class="wrap"><img src="file://{mark}">
<div><h1>{name}</h1><p>{tag}</p><div class="rule"></div></div></div></body></html>"""


def shot(html_path, dest, w, h):
    subprocess.run([CHROME, "--headless", "--disable-gpu", f"--screenshot={dest}",
                    f"--window-size={w},{h}", "--force-device-scale-factor=1",
                    "--default-background-color=00000000", "--hide-scrollbars",
                    "--allow-file-access-from-files", f"file://{html_path}"],
                   capture_output=True)


class Target:
    def __init__(self, root, tmp):
        self.root = root
        self.tmp = tmp
        os.makedirs(os.path.join(root, "png"), exist_ok=True)
        os.makedirs(tmp, exist_ok=True)

    def svg(self, name, text):
        open(os.path.join(self.root, name), "w").write(text + "\n")

    def png(self, svg_name, size, out_name):
        html = os.path.join(self.tmp, out_name + ".html")
        open(html, "w").write(
            f'<html><head><meta charset="utf-8"><style>html,body{{margin:0;padding:0;'
            f'background:transparent}}img{{display:block;width:{size}px;height:{size}px}}'
            f'</style></head><body>'
            f'<img src="file://{os.path.join(self.root, svg_name)}"></body></html>')
        dest = os.path.join(self.root, "png", out_name)
        shot(html, dest, size, size)
        return dest

    def ico(self, frames, name):
        blobs = [(s, open(p, "rb").read()) for s, p in frames]
        entries, data = b"", b""
        offset = 6 + 16 * len(blobs)
        for s, blob in blobs:
            entries += struct.pack("<BBBBHHII", s if s < 256 else 0, s if s < 256 else 0,
                                   0, 0, 1, 32, len(blob), offset)
            offset += len(blob)
            data += blob
        open(os.path.join(self.root, name), "wb").write(
            struct.pack("<HHH", 0, 1, len(blobs)) + entries + data)

    def social(self, mark_svg, name, tag, out_name):
        html = os.path.join(self.tmp, out_name + ".html")
        open(html, "w").write(SOCIAL.format(mark=os.path.join(self.root, mark_svg),
                                            name=name, tag=tag))
        shot(html, os.path.join(self.root, out_name), 1280, 640)


def build_umwelt(t):
    t.svg("umwelt-mark-light.svg", M.render(INK, ACCENT, title="umwelt"))
    t.svg("umwelt-mark-dark.svg", M.render(LIGHT, ACCENT, 0.42, 0.28, title="umwelt"))
    t.svg("umwelt-mark-small-light.svg", M.render_small(INK, ACCENT, title="umwelt"))
    t.svg("umwelt-mark-small-dark.svg", M.render_small(LIGHT, ACCENT, title="umwelt"))
    t.svg("umwelt-tile.svg", M.render(LIGHT, ACCENT, 0.42, 0.28, tile=INK,
                                      tile_radius=TILE_RX, scale=TILE_SCALE, title="umwelt"))
    t.svg("umwelt-tile-square.svg", M.render(LIGHT, ACCENT, 0.42, 0.28, tile=INK,
                                             scale=TILE_SCALE, title="umwelt"))
    t.svg("umwelt-tile-small.svg", M.render_small(LIGHT, ACCENT, tile=INK,
                                                  tile_radius=TILE_RX, scale=SMALL_SCALE,
                                                  title="umwelt"))
    t.svg("umwelt-tile-micro.svg", M.render_micro(LIGHT, ACCENT, tile=INK,
                                                  tile_radius=TILE_RX, scale=MICRO_SCALE,
                                                  title="umwelt"))
    t.svg("favicon.svg", M.render_small(INK, ACCENT, title="umwelt",
                                        style=M.adaptive_style()))
    frames = []
    for s in ICON_SIZES:
        src = ("umwelt-tile-micro.svg" if s < MICRO_CUT
               else "umwelt-tile-small.svg" if s < SMALL_CUT else "umwelt-tile.svg")
        p = t.png(src, s, f"umwelt-icon-{s}.png")
        if s in ICO_SIZES:
            frames.append((s, p))
    for s in MARK_SIZES:
        t.png("umwelt-tile-square.svg", s, f"umwelt-avatar-{s}.png")
        t.png("umwelt-mark-light.svg", s, f"umwelt-mark-light-{s}.png")
        t.png("umwelt-mark-dark.svg", s, f"umwelt-mark-dark-{s}.png")
    t.ico(frames, "favicon.ico")
    t.social("umwelt-mark-dark.svg", "umwelt",
             "Interest management for real-time simulation: spatial subscription, "
             "priority accumulation, and per-viewer bandwidth budgeting.",
             "umwelt-social-preview.png")


def build_herd(t):
    t.svg("herd-mark-light.svg", M.render_herd(INK, ACCENT, title="herd"))
    t.svg("herd-mark-dark.svg", M.render_herd(LIGHT, ACCENT, 0.62, 0.42, title="herd"))
    t.svg("herd-tile.svg", M.render_herd(LIGHT, ACCENT, 0.62, 0.42, tile=INK,
                                         tile_radius=TILE_RX, scale=TILE_SCALE, title="herd"))
    t.svg("herd-tile-square.svg", M.render_herd(LIGHT, ACCENT, 0.62, 0.42, tile=INK,
                                                scale=TILE_SCALE, title="herd"))
    t.svg("herd-tile-small.svg", M.render_herd_small(LIGHT, ACCENT, tile=INK,
                                                     tile_radius=TILE_RX, scale=SMALL_SCALE,
                                                     title="herd"))
    t.svg("herd-tile-micro.svg", M.render_herd_micro(LIGHT, ACCENT, tile=INK,
                                                     tile_radius=TILE_RX, scale=MICRO_SCALE,
                                                     title="herd"))
    t.svg("favicon.svg", M.render_herd_small(INK, ACCENT, title="herd",
                                             style=M.adaptive_style()))
    frames = []
    for s in ICON_SIZES:
        src = ("herd-tile-micro.svg" if s < MICRO_CUT
               else "herd-tile-small.svg" if s < SMALL_CUT else "herd-tile.svg")
        p = t.png(src, s, f"herd-icon-{s}.png")
        if s in ICO_SIZES:
            frames.append((s, p))
    for s in MARK_SIZES:
        t.png("herd-tile-square.svg", s, f"herd-avatar-{s}.png")
        t.png("herd-mark-light.svg", s, f"herd-mark-light-{s}.png")
        t.png("herd-mark-dark.svg", s, f"herd-mark-dark-{s}.png")
    t.ico(frames, "favicon.ico")
    t.social("herd-mark-dark.svg", "herd",
             "A minimal game, used only to generate load for umwelt. "
             "Never published to crates.io.", "herd-social-preview.png")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--herd", default=DEFAULT_HERD,
                    help="where to write herd's assets; skipped if absent")
    ap.add_argument("--skip-herd", action="store_true")
    args = ap.parse_args()
    tmp = os.path.join(HERE, ".render")
    build_umwelt(Target(HERE, tmp))
    print(f"umwelt assets written to {HERE}")
    if not args.skip_herd:
        parent = os.path.dirname(os.path.dirname(args.herd))
        if os.path.isdir(parent):
            build_herd(Target(args.herd, tmp))
            print(f"herd assets written to {args.herd}")
        else:
            print(f"herd checkout not found at {parent}, skipped")
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
