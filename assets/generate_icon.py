"""Generates the TVH Client app icon + logo lockup.
Run once with Pillow (`pip install pillow`); outputs land in this
directory. Not part of the Rust build - just the source of the
committed PNG/ICO assets.
"""
import numpy as np
from PIL import Image, ImageDraw, ImageFont

SS = 4  # supersample factor for clean anti-aliased edges
SIZE = 1024
CANVAS = SIZE * SS

# Palette
BG_TOP = np.array([56, 132, 255])     # bright blue
BG_BOTTOM = np.array([13, 20, 40])    # near-black navy
SCREEN = (247, 249, 252)              # near-white
PLAY = (255, 122, 26)                 # warm orange accent
ARC = (255, 255, 255, 130)            # translucent white

def diagonal_gradient(size, top, bottom):
    t = np.linspace(0, 1, size)
    xx, yy = np.meshgrid(t, t)
    d = (xx + yy) / 2
    d = d[..., None]
    grad = top[None, None, :] * (1 - d) + bottom[None, None, :] * d
    return Image.fromarray(grad.astype(np.uint8), "RGB")

def rounded_mask(size, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
    return mask

def build_icon():
    grad = diagonal_gradient(CANVAS, BG_TOP, BG_BOTTOM).convert("RGBA")
    bg_mask = rounded_mask(CANVAS, int(CANVAS * 0.22))
    icon = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    icon.paste(grad, (0, 0), bg_mask)

    draw = ImageDraw.Draw(icon)

    # Subtle top-left highlight for depth.
    highlight = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    hd = ImageDraw.Draw(highlight)
    hd.ellipse(
        [-CANVAS * 0.3, -CANVAS * 0.35, CANVAS * 0.75, CANVAS * 0.55],
        fill=(255, 255, 255, 28),
    )
    highlight.putalpha(Image.composite(highlight.split()[3], Image.new("L", (CANVAS, CANVAS), 0), bg_mask))
    icon = Image.alpha_composite(icon, highlight)
    draw = ImageDraw.Draw(icon)

    # "Screen": rounded rect, centered, 16:10-ish.
    screen_w = CANVAS * 0.62
    screen_h = CANVAS * 0.42
    sx0 = (CANVAS - screen_w) / 2
    sy0 = (CANVAS - screen_h) / 2 + CANVAS * 0.02
    sx1 = sx0 + screen_w
    sy1 = sy0 + screen_h
    screen_radius = screen_h * 0.16
    draw.rounded_rectangle([sx0, sy0, sx1, sy1], radius=screen_radius, fill=SCREEN)

    # Play triangle, centered in the screen.
    tri_h = screen_h * 0.46
    tri_w = tri_h * 0.9
    cx = (sx0 + sx1) / 2 + tri_w * 0.08  # tiny optical shift right
    cy = (sy0 + sy1) / 2
    pts = [
        (cx - tri_w / 2, cy - tri_h / 2),
        (cx - tri_w / 2, cy + tri_h / 2),
        (cx + tri_w / 2, cy),
    ]
    draw.polygon(pts, fill=PLAY)

    # Broadcast-signal arcs, top-right of the badge (outside the screen).
    arc_layer = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    ad = ImageDraw.Draw(arc_layer)
    ax = CANVAS * 0.80
    ay = CANVAS * 0.17
    for i, r in enumerate([CANVAS * 0.10, CANVAS * 0.155, CANVAS * 0.21]):
        w = int(CANVAS * 0.018)
        alpha = 235 - i * 55
        ad.arc([ax - r, ay - r, ax + r, ay + r], start=300, end=390, fill=(255, 255, 255, alpha), width=w)
    icon = Image.alpha_composite(icon, arc_layer)

    icon = icon.resize((SIZE, SIZE), Image.LANCZOS)
    return icon

def build_logo(icon_1024):
    mark_size = 220
    mark = icon_1024.resize((mark_size, mark_size), Image.LANCZOS)

    pad = 28
    gap = 26
    text = "TVH Client"
    font = ImageFont.truetype(
        "/usr/share/fonts/truetype/liberation2/LiberationSans-Bold.ttf", 118
    )
    sub_font = ImageFont.truetype(
        "/usr/share/fonts/truetype/liberation2/LiberationSans-Bold.ttf", 34
    )

    dummy = Image.new("RGBA", (10, 10))
    dd = ImageDraw.Draw(dummy)
    text_bbox = dd.textbbox((0, 0), text, font=font)
    text_w = text_bbox[2] - text_bbox[0]
    text_h = text_bbox[3] - text_bbox[1]
    sub = "TVHeadend desktop klient"
    sub_bbox = dd.textbbox((0, 0), sub, font=sub_font)
    sub_w = sub_bbox[2] - sub_bbox[0]

    width = pad * 2 + mark_size + gap + max(text_w, sub_w)
    height = pad * 2 + mark_size
    logo = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    logo.paste(mark, (pad, pad), mark)

    d = ImageDraw.Draw(logo)
    text_x = pad + mark_size + gap
    block_h = text_h + 14 + (sub_bbox[3] - sub_bbox[1])
    text_y = pad + (mark_size - block_h) / 2 - text_bbox[1]
    d.text((text_x, text_y), text, font=font, fill=(15, 23, 42, 255))
    sub_y = text_y + text_h + 20 - text_bbox[1] + text_bbox[1]
    sub_y = pad + (mark_size - block_h) / 2 + text_h + 14 - sub_bbox[1]
    d.text((text_x, sub_y), sub, font=sub_font, fill=(100, 116, 139, 255))
    return logo

if __name__ == "__main__":
    icon = build_icon()
    icon.save("icon-1024.png")
    for s in (16, 24, 32, 48, 64, 128, 256):
        icon.resize((s, s), Image.LANCZOS).save(f"icon-{s}.png")
    icon.save(
        "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    logo = build_logo(icon)
    logo.save("logo.png")

    print("done")
