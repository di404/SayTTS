"""Generate the SayTTS app icon (all sizes Tauri bundles) from code.

Five rounded bars as a voice waveform, one bar in an accent color.
Pick a palette with VARIANT; rerun to regenerate every size.

    python3 voice-relay/app/icons/gen_icon.py
"""

from PIL import Image, ImageDraw, ImageFilter, ImageChops

S = 1024
K = 2
BODY = (100, 100, 924, 924, 186)

# Hand-tuned, not symmetric: reads like a real syllable, peak right of center.
HEIGHTS = [0.34, 0.66, 0.50, 1.0, 0.42]
BAR_W, GAP, MAXH = 66, 46, 430


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def body_fill(top, bottom, grain=0):
    n = 64
    g = Image.new("RGB", (1, n))
    for y in range(n):
        g.putpixel((0, y), lerp(top, bottom, y / (n - 1)))
    g = g.resize((S * K, S * K), Image.BICUBIC)
    if grain:
        noise = Image.effect_noise((S * K // 2, S * K // 2), 64).resize((S * K, S * K))
        noise = noise.point(lambda v: 128 + (v - 128) * grain // 64)
        g = ImageChops.overlay(g, Image.merge("RGB", [noise] * 3))
    return g.convert("RGBA")


def render(top, bottom, bar, accent=None, accent_i=3, grain=0, shadow=None):
    W = S * K
    img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    m = Image.new("L", (W, W), 0)
    x0, y0, x1, y1, r = BODY
    ImageDraw.Draw(m).rounded_rectangle([x0 * K, y0 * K, x1 * K, y1 * K], radius=r * K, fill=255)
    img.paste(body_fill(top, bottom, grain), (0, 0), m)

    total = len(HEIGHTS) * BAR_W + (len(HEIGHTS) - 1) * GAP
    sx = (S - total) / 2
    rects = []
    for i, h in enumerate(HEIGHTS):
        hh = MAXH * h
        bx = sx + i * (BAR_W + GAP)
        rects.append((i, (bx, 512 - hh / 2, bx + BAR_W, 512 + hh / 2)))

    if shadow:
        sh = Image.new("RGBA", (W, W), (0, 0, 0, 0))
        sd = ImageDraw.Draw(sh)
        for i, (a, b, c, d) in rects:
            sd.rounded_rectangle([a * K, (b + 10) * K, c * K, (d + 10) * K], radius=BAR_W / 2 * K, fill=shadow)
        sh = sh.filter(ImageFilter.GaussianBlur(14 * K))
        img = Image.composite(Image.alpha_composite(img, sh), img, m)

    d = ImageDraw.Draw(img)
    for i, (a, b, c, e) in rects:
        col = accent if (accent and i == accent_i) else bar
        d.rounded_rectangle([a * K, b * K, c * K, e * K], radius=BAR_W / 2 * K, fill=col)
    return img.resize((S, S), Image.LANCZOS)


VARIANTS = {
    "paper": dict(top=(246, 242, 234), bottom=(236, 230, 219), bar=(28, 27, 31),
                  accent=(232, 74, 44), grain=10, shadow=(60, 40, 20, 40)),
    "ink": dict(top=(40, 38, 64), bottom=(22, 21, 38), bar=(244, 238, 226),
                accent=(255, 122, 89), grain=12, shadow=(0, 0, 0, 90)),
    "tomato": dict(top=(255, 104, 72), bottom=(238, 72, 46), bar=(255, 250, 244),
                   accent=None, grain=8, shadow=(120, 20, 0, 70)),
}

VARIANT = "paper"


def export():
    import subprocess, tempfile
    from pathlib import Path
    here = Path(__file__).resolve().parent
    master = render(**VARIANTS[VARIANT])
    for name, px in (("icon.png", 512), ("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)):
        master.resize((px, px), Image.LANCZOS).save(here / name)
    master.save(here / "icon.ico", sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (256, 256)])
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "icon.iconset"
        iconset.mkdir()
        for base in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                suffix = "@2x" if scale == 2 else ""
                master.resize((base * scale,) * 2, Image.LANCZOS).save(iconset / f"icon_{base}x{base}{suffix}.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(here / "icon.icns")], check=True)


def preview_sheet(out):
    imgs = [render(**v) for v in VARIANTS.values()]
    sheet = Image.new("RGBA", (3 * 360 + 4 * 30, 360 + 100), (250, 250, 250, 255))
    for i, im in enumerate(imgs):
        sm = im.resize((360, 360), Image.LANCZOS)
        sheet.paste(sm, (30 + i * 390, 30), sm)
        t = im.resize((32, 32), Image.LANCZOS)
        sheet.paste(t, (30 + i * 390 + 164, 410), t)
    sheet.save(out)


if __name__ == "__main__":
    import sys
    if len(sys.argv) > 1:
        preview_sheet(sys.argv[1])  # compare all variants
    else:
        export()
