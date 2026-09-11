#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""Build the browser / home-screen / thumbnail icon set from the HIL logo.

The logo is a wide mark (720x320) on white. Tabs, home-screen icons and link
thumbnails all want a SQUARE image, so it is centred on a white square with a
margin -- padded, never stretched, because stretching distorts the letterforms.

    pip install pillow
    python tools/make-icons.py "D:/.../HIL logo.jpg"
    python tools/make-icons.py "D:/.../HIL logo.jpg" --emblem "D:/.../snu_ui_download.png"

Writes into web/icons/ :
    favicon.ico              16/32/48 multi-size, for browser tabs
    icon-192.png             Android home screen / PWA
    icon-512.png             splash screen + high-DPI thumbnails
    icon-maskable-512.png    Android adaptive icon (20% safe zone)
    apple-touch-icon.png     180x180 iOS home screen (alpha not allowed)
    og-image.png             1200x630 link-preview card
    logo.png                 the mark itself, for in-page use
    emblem.png               SNU emblem, 128px (only with --emblem)
    site.webmanifest
"""
import os
import sys

try:
    from PIL import Image
except ImportError:
    sys.exit("Pillow is required:  pip install pillow")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "web", "icons")
WHITE = (255, 255, 255, 255)


def flatten(img):
    """Composite onto white.

    iOS rejects alpha in apple-touch-icon, and a transparent favicon becomes a
    dark smudge against dark browser chrome. Near-white pixels are snapped to
    pure white as well, so JPEG ringing around the mark does not show up as a
    grey halo once it sits on the white square.
    """
    img = img.convert("RGBA")
    bg = Image.new("RGBA", img.size, WHITE)
    bg.alpha_composite(img)
    px = bg.load()
    w, h = bg.size
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            if r > 246 and g > 246 and b > 246:
                px[x, y] = WHITE
    return bg


def trim(img):
    """Drop the uniform white border so our padding is predictable."""
    mask = img.convert("RGB").point(lambda v: 255 if v < 245 else 0).convert("L")
    bbox = mask.getbbox()
    return img.crop(bbox) if bbox else img


def square(img, size, pad_ratio=0.08):
    """Centre the mark on a white square, scaled to fit inside the margin."""
    canvas = Image.new("RGBA", (size, size), WHITE)
    inner = max(1, int(size * (1 - 2 * pad_ratio)))
    w, h = img.size
    scale = min(inner / w, inner / h)
    new = img.resize((max(1, round(w * scale)), max(1, round(h * scale))),
                     Image.LANCZOS)
    canvas.alpha_composite(new, ((size - new.width) // 2, (size - new.height) // 2))
    return canvas


MANIFEST = """{
  "name": "HIL GPU Monitor",
  "short_name": "HIL GPU",
  "start_url": ".",
  "scope": ".",
  "display": "standalone",
  "background_color": "#ffffff",
  "theme_color": "#0d1117",
  "icons": [
    { "src": "icons/icon-192.png", "sizes": "192x192", "type": "image/png" },
    { "src": "icons/icon-512.png", "sizes": "512x512", "type": "image/png" },
    { "src": "icons/icon-maskable-512.png", "sizes": "512x512",
      "type": "image/png", "purpose": "maskable" }
  ]
}
"""


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    src = args[0]
    emblem_src = None
    if "--emblem" in args:
        i = args.index("--emblem")
        if i + 1 >= len(args):
            sys.exit("--emblem needs a path")
        emblem_src = args[i + 1]
    for p in filter(None, (src, emblem_src)):
        if not os.path.isfile(p):
            sys.exit("no such file: %s" % p)

    os.makedirs(OUT, exist_ok=True)
    logo = trim(flatten(Image.open(src)))
    print("logo %s -> %dx%d (%.2f:1)" % (
        os.path.basename(src), logo.width, logo.height, logo.width / logo.height))

    made = []

    def save(img, name, **kw):
        p = os.path.join(OUT, name)
        img.convert("RGB").save(p, **kw)
        made.append((name, os.path.getsize(p)))

    # Tab icons are tiny; a smaller margin buys back precious pixels.
    ico_path = os.path.join(OUT, "favicon.ico")
    square(logo, 48, 0.03).convert("RGB").save(
        ico_path, format="ICO", sizes=[(16, 16), (32, 32), (48, 48)])
    made.append(("favicon.ico", os.path.getsize(ico_path)))

    save(square(logo, 192), "icon-192.png", format="PNG", optimize=True)
    save(square(logo, 512), "icon-512.png", format="PNG", optimize=True)
    # Android crops maskable icons to a circle: keep a 20% safe zone.
    save(square(logo, 512, 0.20), "icon-maskable-512.png", format="PNG", optimize=True)
    save(square(logo, 180), "apple-touch-icon.png", format="PNG", optimize=True)

    og = Image.new("RGBA", (1200, 630), WHITE)
    fit = min(940 / logo.width, 420 / logo.height)
    m = logo.resize((round(logo.width * fit), round(logo.height * fit)), Image.LANCZOS)
    og.alpha_composite(m, ((1200 - m.width) // 2, (630 - m.height) // 2))
    save(og, "og-image.png", format="PNG", optimize=True)

    save(logo.resize((round(logo.width * 80 / logo.height), 80), Image.LANCZOS),
         "logo.png", format="PNG", optimize=True)

    if emblem_src:
        em = trim(flatten(Image.open(emblem_src)))
        save(square(em, 128, 0.0), "emblem.png", format="PNG", optimize=True)
        print("emblem %s -> %dx%d" % (os.path.basename(emblem_src), em.width, em.height))

    with open(os.path.join(OUT, "site.webmanifest"), "w", encoding="utf-8") as f:
        f.write(MANIFEST)
    made.append(("site.webmanifest", os.path.getsize(os.path.join(OUT, "site.webmanifest"))))

    print("\nwrote %s:" % OUT)
    for name, size in made:
        print("  %-26s %6.1f KB" % (name, size / 1024))


if __name__ == "__main__":
    main()
