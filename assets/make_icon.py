"""Renders the ScanDesk app icon: a flatbed scanner with its lid open,
a document on the glass and a glowing scan beam.

Usage: python3 assets/make_icon.py
Writes assets/icon.png (1024 px), assets/icon-256.png and assets/AppIcon.icns.
"""

import os
import shutil
import subprocess
import tempfile

from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 2  # supersampling factor
N = 1024 * S
HERE = os.path.dirname(os.path.abspath(__file__))


def p(*pts):
    return [(x * S, y * S) for x, y in pts]


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def gradient(c1, c2, box, horizontal=False):
    """RGBA image of canvas size with a linear gradient across `box`."""
    x0, y0, x1, y1 = [v * S for v in box]
    img = Image.new("RGBA", (N, N))
    d = ImageDraw.Draw(img)
    if horizontal:
        for x in range(N):
            t = min(max((x - x0) / max(x1 - x0, 1), 0), 1)
            d.line([(x, 0), (x, N)], fill=lerp(c1, c2, t))
    else:
        for y in range(N):
            t = min(max((y - y0) / max(y1 - y0, 1), 0), 1)
            d.line([(0, y), (N, y)], fill=lerp(c1, c2, t))
    return img


def fill_poly(canvas, pts, fill, radius=0):
    """Paints a polygon with a colour or a gradient image."""
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).polygon(p(*pts), fill=255)
    if radius:
        mask = mask.filter(ImageFilter.GaussianBlur(radius * S))
    paint(canvas, fill, mask)


def fill_rrect(canvas, box, r, fill):
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).rounded_rectangle([v * S for v in box], r * S, fill=255)
    paint(canvas, fill, mask)
    return mask


def paint(canvas, fill, mask):
    if isinstance(fill, Image.Image):
        layer = fill
        if layer.mode == "RGBA":
            mask = ImageChops.multiply(mask, layer.getchannel("A"))
    else:
        layer = Image.new("RGBA", (N, N), fill)
        if len(fill) == 4:
            mask = ImageChops.multiply(mask, Image.new("L", (N, N), fill[3]))
    canvas.paste(layer, (0, 0), mask)


def glow(canvas, draw_fn, blur, color, strength=255):
    mask = Image.new("L", (N, N), 0)
    draw_fn(ImageDraw.Draw(mask))
    mask = mask.filter(ImageFilter.GaussianBlur(blur * S))
    if strength < 255:
        mask = mask.point(lambda v: v * strength // 255)
    canvas.paste(Image.new("RGBA", (N, N), color), (0, 0), mask)


def render():
    img = Image.new("RGBA", (N, N), (0, 0, 0, 0))

    # --- background tile (macOS icon grid: 824 px body, 100 px margin) ------
    tile = (100, 100, 924, 924)
    shadow = Image.new("L", (N, N), 0)
    ImageDraw.Draw(shadow).rounded_rectangle([v * S for v in (110, 118, 914, 936)], 185 * S, fill=110)
    shadow = shadow.filter(ImageFilter.GaussianBlur(14 * S))
    img.paste(Image.new("RGBA", (N, N), (0, 0, 0, 255)), (0, 0), shadow)
    tile_mask = fill_rrect(img, tile, 185, gradient((37, 99, 235), (15, 23, 82), (0, 100, 0, 924)))
    # soft light from the top
    light = Image.new("L", (N, N), 0)
    ImageDraw.Draw(light).ellipse([v * S for v in (-60, -420, 1084, 520)], fill=70)
    light = ImageChops.multiply(light.filter(ImageFilter.GaussianBlur(60 * S)), tile_mask)
    img.paste(Image.new("RGBA", (N, N), (255, 255, 255, 255)), (0, 0), light)

    # --- shadow under the scanner ----------------------------------------------
    sh = Image.new("L", (N, N), 0)
    ImageDraw.Draw(sh).ellipse([v * S for v in (170, 735, 854, 835)], fill=150)
    sh = ImageChops.multiply(sh.filter(ImageFilter.GaussianBlur(22 * S)), tile_mask)
    img.paste(Image.new("RGBA", (N, N), (5, 10, 40, 255)), (0, 0), sh)

    # --- lid (open, hinged at the back edge) -------------------------------------
    hinge_l, hinge_r = (262, 548), (762, 548)
    lid_top_l, lid_top_r = (292, 268), (732, 268)
    # outer shell of the lid (back side visible as a thin rim)
    fill_poly(img, [(hinge_l[0] - 14, hinge_l[1]), (hinge_r[0] + 14, hinge_r[1]),
                    (lid_top_r[0] + 12, lid_top_r[1] - 14), (lid_top_l[0] - 12, lid_top_l[1] - 14)],
              gradient((203, 213, 225), (148, 163, 184), (0, 254, 0, 548)))
    # white document pad on the inside of the lid
    fill_poly(img, [(hinge_l[0] + 6, hinge_l[1] - 10), (hinge_r[0] - 6, hinge_r[1] - 10),
                    (lid_top_r[0] - 8, lid_top_r[1] + 10), (lid_top_l[0] + 8, lid_top_l[1] + 10)],
              gradient((226, 232, 240), (248, 250, 252), (0, 278, 0, 538)))
    # hinges
    for x in (330, 694):
        fill_rrect(img, (x - 34, 536, x + 34, 562), 8, (100, 116, 139, 255))

    # --- scanner body ---------------------------------------------------------
    top = [(232, 548), (792, 548), (862, 668), (162, 668)]
    fill_poly(img, top, gradient((241, 245, 249), (203, 213, 225), (0, 548, 0, 668)))
    # front face with rounded bottom
    front = (162, 664, 862, 758)
    fill_rrect(img, front, 26, gradient((148, 163, 184), (71, 85, 105), (0, 664, 0, 758)))
    fill_poly(img, [(162, 664), (862, 664), (862, 700), (162, 700)],
              gradient((148, 163, 184), (120, 136, 158), (0, 664, 0, 700)))
    # bright edge between top and front
    fill_poly(img, [(162, 664), (862, 664), (860, 671), (164, 671)], (255, 255, 255, 170))

    # glass plate
    glass = [(272, 562), (752, 562), (806, 652), (218, 652)]
    fill_poly(img, glass, gradient((30, 41, 59), (15, 23, 42), (0, 562, 0, 652)))

    # document lying on the glass
    doc = [(318, 570), (602, 570), (630, 646), (282, 646)]
    fill_poly(img, doc, (250, 250, 252, 255))
    d = ImageDraw.Draw(img)
    for i, (y, w) in enumerate([(584, 0.55), (598, 0.85), (611, 0.8), (624, 0.9), (636, 0.6)]):
        t = (y - 570) / 76
        xl = 318 + (282 - 318) * t + 22
        xr = 602 + (630 - 602) * t - 22
        d.line(p((xl, y), (xl + (xr - xl) * w, y)), fill=(148, 163, 184, 255), width=int(4 * S if i else 6 * S))

    # reflection on the glass
    fill_poly(img, [(560, 562), (640, 562), (560, 652), (480, 652)], (255, 255, 255, 28))

    # scan beam with glow
    by = 612
    t = (by - 562) / 90
    bl = 272 + (218 - 272) * t
    br = 752 + (806 - 752) * t
    glow(img, lambda g: g.line(p((bl, by), (br, by)), fill=255, width=26 * S), 14, (56, 189, 248, 255), 150)
    glow(img, lambda g: g.line(p((bl, by), (br, by)), fill=255, width=10 * S), 4, (125, 211, 252, 255), 220)
    ImageDraw.Draw(img).line(p((bl + 4, by), (br - 4, by)), fill=(240, 249, 255, 255), width=5 * S)

    # --- front panel: status light and buttons ------------------------------------
    glow(img, lambda g: g.ellipse(p((214, 700), (246, 732)), fill=255), 8, (74, 222, 128, 255))
    ImageDraw.Draw(img).ellipse(p((221, 707), (239, 725)), fill=(187, 247, 208, 255))
    for x in (712, 778):
        fill_rrect(img, (x - 26, 702, x + 26, 728), 13, (51, 65, 85, 255))
        fill_rrect(img, (x - 22, 704, x + 22, 722), 11, (100, 116, 139, 255))

    return img.resize((1024, 1024), Image.LANCZOS)


def main():
    icon = render()
    icon.save(os.path.join(HERE, "icon.png"))
    icon.resize((256, 256), Image.LANCZOS).save(os.path.join(HERE, "icon-256.png"))

    if shutil.which("iconutil"):
        with tempfile.TemporaryDirectory() as tmp:
            iconset = os.path.join(tmp, "AppIcon.iconset")
            os.mkdir(iconset)
            for size in (16, 32, 128, 256, 512):
                icon.resize((size, size), Image.LANCZOS).save(os.path.join(iconset, f"icon_{size}x{size}.png"))
                icon.resize((size * 2, size * 2), Image.LANCZOS).save(
                    os.path.join(iconset, f"icon_{size}x{size}@2x.png"))
            subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(HERE, "AppIcon.icns")], check=True)
    print("icon written")


if __name__ == "__main__":
    main()
