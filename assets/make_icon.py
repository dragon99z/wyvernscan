#!/usr/bin/env python3
"""Builds every icon artifact from the source files in this folder.

    pip install cairosvg pillow numpy scipy
    python3 assets/make_icon.py

Sources:
  icon_base.svg      the badge: black ring and green field (a minimal take on the
                     original dragon99z logo) -- no creature
  icon_stripes.svg   the three white stripes
  dragon_head.png    the dragon artwork (transparent background)

How the pieces are combined, in drawing order:
  1. badge
  2. stripes, except inside the dragon's open mouth, so they read as passing
     behind the head
  3. the dragon: clipped by the ring on the left (which also hides the artwork's
     cut-off neck stump), with the snout breaking out past the ring on the right
  4. the horns, shortened and drawn unclipped so they break out over the ring

The artwork's horns are extremely long and thin (about 12:1), far longer than the
badge can hold, and the neck stump forces the head to sit left-of-centre. Rather
than fading the horns out, they are squeezed horizontally toward their base: that
keeps their real outline and pointed tip and just gives them a normal length.

The numbers below are tuned by eye; if you swap the artwork, re-tune them.

Outputs (all committed, so a normal build needs neither Python nor these deps):
  icon-512.png   README / store artwork
  icon.ico       embedded into wyvernscan.exe by build.rs (Windows Explorer, taskbar)
  icon-256.rgba  raw RGBA, 256x256 -- the window icon (include_bytes! in main.rs)
  icon-96.rgba   raw RGBA, 96x96   -- the in-app logo texture (include_bytes! in app.rs)

Raw RGBA is used so the app needs no image-decoding dependency just to show a logo.
"""
import io
import os

import cairosvg
import numpy as np
from PIL import Image, ImageChops, ImageDraw, ImageFilter
from scipy import ndimage as ndi

HERE = os.path.dirname(os.path.abspath(__file__))
MASTER = 1024

# Badge geometry, matching icon_base.svg.
CX, CY, R_INNER = 500, 530, 416

# Head placement on the 1024 master. Source-art coordinates: the neck stump's
# last straight cut ends at x=640, the eye sits at (1250, 315), and the snout and
# fangs are right of x=1400.
HEAD_SCALE = 0.72
STUMP_EDGE_X = 130    # where the stump's last cut lands; must stay outside the ring's inner edge
RISE = 30             # lifts the head to leave room for the stripes under the jaw
SNOUT_X_SRC = 1400

# Horns: everything above the head's fur and left of HORN_ANCHOR_X is the horn
# layer; it is compressed toward the anchor by HORN_KEEP (1.0 = original length).
HORN_ANCHOR_X = 880
HORN_KEEP = 0.42
HORN_EXTRA = 40   # source px right of the anchor that travel with the horn, unsqueezed

# The mouth interior is the transparent region that stays enclosed once the
# opening is closed off; it is found at this clearance (source px) and grown back.
MOUTH_CLEARANCE = 30
MOUTH_GROW = 36


def render_svg(name: str) -> Image.Image:
    png = cairosvg.svg2png(
        url=os.path.join(HERE, name), output_width=MASTER, output_height=MASTER
    )
    return Image.open(io.BytesIO(png)).convert("RGBA")


def split_horn(head: Image.Image):
    """Returns (head without horns, horn layer) in source coordinates."""
    a = np.array(head)
    r, g, b, al = (a[:, :, i].astype(int) for i in range(4))
    cream = (al > 200) & (r > 200) & (g > 195) & (b > 140) & (b < 235)

    width = head.width
    bottom = np.full(width, -1)
    for x in range(width):
        ys = np.where(cream[:230, x])[0]
        if len(ys):
            bottom[x] = ys.max() + 16     # cream plus the horn's lower outline
    # Thin tip columns have no cream: fall back to any opaque pixel above the fur.
    for x in range(0, 140):
        ys = np.where(al[:230, x] > 128)[0]
        bottom[x] = ys.max() + 2 if len(ys) else -1
    known = bottom >= 0
    bottom = np.interp(np.arange(width), np.where(known)[0], bottom[known])
    bottom = ndi.median_filter(bottom, size=15)

    yy, xx = np.mgrid[0:head.height, 0:width]
    # Fur is dark green; the horn is cream with a black outline. Keeping green
    # pixels out stops slivers of fur being dragged along with the horn.
    fur = (g - r > 14) & (g - b > 6)
    horn = (al > 0) & (yy <= bottom[xx]) & (xx < HORN_ANCHOR_X + HORN_EXTRA) & ~fur

    # The two layers partition the original art exactly (every pixel is in one
    # or the other), which is what lets them be summed back together later
    # without a hairline seam.
    horn_rgba = a.copy()
    horn_rgba[~horn] = 0
    rest_rgba = a.copy()
    rest_rgba[horn] = 0
    return Image.fromarray(rest_rgba, "RGBA"), Image.fromarray(horn_rgba, "RGBA")


def premultiplied(img: Image.Image) -> np.ndarray:
    """float HxWx4 with colour already multiplied by alpha, so layers can be added."""
    arr = np.asarray(img.convert("RGBA"), dtype=np.float32) / 255.0
    arr[:, :, :3] *= arr[:, :, 3:4]
    return arr


def resize_pm(arr: np.ndarray, size) -> np.ndarray:
    """Lanczos resize of a premultiplied array (resampling is linear, so it is
    valid per channel and keeps layers summable)."""
    chans = [
        np.asarray(Image.fromarray(arr[:, :, c], "F").resize(size, Image.LANCZOS))
        for c in range(4)
    ]
    return np.clip(np.stack(chans, axis=2), 0.0, 1.0)


def squeeze_horn(horn: Image.Image) -> np.ndarray:
    """Compresses the horn toward its base. The part right of the anchor is
    copied unsqueezed, so the horn stays one continuous piece."""
    pm = premultiplied(horn)
    h, w = pm.shape[:2]
    new_w = round(HORN_ANCHOR_X * HORN_KEEP)
    squeezed = resize_pm(pm[:, :HORN_ANCHOR_X], (new_w, h))
    out = np.zeros_like(pm)
    out[:, HORN_ANCHOR_X - new_w:HORN_ANCHOR_X] = squeezed
    out[:, HORN_ANCHOR_X:] = pm[:, HORN_ANCHOR_X:]
    return out


def mouth_mask(head: Image.Image) -> Image.Image:
    """L mask (255 = mouth interior) in source coordinates."""
    transparent = np.array(head)[:, :, 3] < 128
    depth = ndi.distance_transform_edt(transparent)
    labels, count = ndi.label(depth > MOUTH_CLEARANCE)
    edge = set(np.unique(np.concatenate(
        [labels[0, :], labels[-1, :], labels[:, 0], labels[:, -1]]))) - {0}
    enclosed = [i for i in range(1, count + 1) if i not in edge]
    if not enclosed:
        raise RuntimeError("mouth interior not found; re-tune MOUTH_CLEARANCE")
    core = labels == max(enclosed, key=lambda i: (labels == i).sum())
    near = ndi.distance_transform_edt(~core) <= MOUTH_GROW
    return Image.fromarray(((near & transparent) * 255).astype("uint8"), "L")


def build_master() -> Image.Image:
    s = HEAD_SCALE
    head = Image.open(os.path.join(HERE, "dragon_head.png")).convert("RGBA")
    mouth_src = mouth_mask(head)
    body_src, horn_src = split_horn(head)
    horn_pm = squeeze_horn(horn_src)

    size = (round(head.width * s), round(head.height * s))
    body_pm = resize_pm(premultiplied(body_src), size)
    horn_pm = resize_pm(horn_pm, size)
    mouth = mouth_src.resize(size, Image.LANCZOS)
    x0 = round(STUMP_EDGE_X - 640 * s)
    y0 = round(CY - RISE - 500 * s)

    def place_pm(arr: np.ndarray) -> np.ndarray:
        canvas = np.zeros((MASTER, MASTER, 4), dtype=np.float32)
        h, w = arr.shape[:2]
        canvas[y0:y0 + h, max(x0, 0):x0 + w] = arr[:, max(-x0, 0):]
        return canvas

    # Where the body may show: inside the ring, plus the snout breaking out.
    clip = Image.new("L", (MASTER, MASTER), 0)
    d = ImageDraw.Draw(clip)
    d.ellipse((CX - R_INNER, CY - R_INNER, CX + R_INNER, CY + R_INNER), fill=255)
    d.rectangle((x0 + round(SNOUT_X_SRC * s), 0, MASTER, MASTER), fill=255)
    clip_f = np.asarray(clip, dtype=np.float32)[:, :, None] / 255.0

    # Body is clipped by the ring; the horn is not. Premultiplied layers simply add.
    creature_pm = np.clip(place_pm(body_pm) * clip_f + place_pm(horn_pm), 0.0, 1.0)
    rgba = creature_pm.copy()
    rgba[:, :, :3] = np.where(
        creature_pm[:, :, 3:4] > 1e-4, creature_pm[:, :, :3] / np.maximum(creature_pm[:, :, 3:4], 1e-4), 0.0
    )
    creature = Image.fromarray((np.clip(rgba, 0, 1) * 255 + 0.5).astype("uint8"), "RGBA")

    mouth_canvas = Image.new("L", (MASTER, MASTER), 0)
    mouth_canvas.paste(mouth, (x0, y0))

    # Stripes, hidden inside the open mouth.
    stripes = render_svg("icon_stripes.svg")
    stripes.putalpha(ImageChops.multiply(
        stripes.getchannel("A"), ImageChops.invert(mouth_canvas)))

    # A soft, light shadow separates the creature from the badge.
    alpha = creature.getchannel("A")
    shadow = Image.new("RGBA", (MASTER, MASTER), (0, 0, 0, 0))
    shadow.putalpha(alpha.point(lambda v: int(v * 0.30)).filter(ImageFilter.GaussianBlur(12)))
    shadow = ImageChops.offset(shadow, 5, 12)

    out = render_svg("icon_base.svg")
    out.alpha_composite(stripes)
    out.alpha_composite(shadow)
    out.alpha_composite(creature)
    return out


def at_size(master: Image.Image, size: int) -> Image.Image:
    img = master.resize((size, size), Image.LANCZOS)
    if size <= 48:
        # Downscaling softens the black outlines; a touch of sharpening keeps
        # the dragon's face readable in the taskbar and title bar.
        img = img.filter(ImageFilter.UnsharpMask(radius=0.6, percent=60, threshold=0))
    return img


def main() -> None:
    master = build_master()
    at_size(master, 512).save(os.path.join(HERE, "icon-512.png"))

    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [at_size(master, s) for s in sizes]
    frames[-1].save(
        os.path.join(HERE, "icon.ico"),
        format="ICO",
        sizes=[(s, s) for s in sizes],
        append_images=frames[:-1],
    )

    for size in (256, 96):
        with open(os.path.join(HERE, f"icon-{size}.rgba"), "wb") as f:
            f.write(at_size(master, size).tobytes())


if __name__ == "__main__":
    main()
