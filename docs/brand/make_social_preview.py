"""Renders docs/brand/social-preview.png (1280x640) for the GitHub social preview."""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).parent
NIGHT = (0x12, 0x14, 0x18)
BLUE = (0x3A, 0x7B, 0xD5)
INK = (0x0C, 0x0E, 0x11)
WHITE = (255, 255, 255)
GHOST = (0x9A, 0x9D, 0xA3)
FONTS = Path(r"C:\Windows\Fonts")


def font(name, size):
    for candidate in (name, "segoeuib.ttf", "arialbd.ttf"):
        p = FONTS / candidate
        if p.exists():
            return ImageFont.truetype(str(p), size)
    return ImageFont.load_default()


def mark(draw, x, y, s):
    """The Tab-key mark with its top-left at (x, y) and size s (same geometry as mark.svg)."""
    u = s / 120
    draw.rounded_rectangle((x + 6 * u, y + 10 * u, x + 114 * u, y + 114 * u), radius=22 * u, fill=INK)
    draw.rounded_rectangle((x + 6 * u, y + 6 * u, x + 114 * u, y + 106 * u), radius=22 * u, fill=BLUE)
    w = int(11 * u)
    draw.line((x + 30 * u, y + 56 * u, x + 78 * u, y + 56 * u), fill=WHITE, width=w)
    draw.line((x + 64 * u, y + 38 * u, x + 84 * u, y + 56 * u), fill=WHITE, width=w)
    draw.line((x + 84 * u, y + 56 * u, x + 64 * u, y + 74 * u), fill=WHITE, width=w)
    draw.line((x + 92 * u, y + 36 * u, x + 92 * u, y + 76 * u), fill=WHITE, width=w)
    r = w // 2
    for cx, cy in ((30, 56), (78, 56), (64, 38), (84, 56), (64, 74), (92, 36), (92, 76)):
        draw.ellipse((x + cx * u - r, y + cy * u - r, x + cx * u + r, y + cy * u + r), fill=WHITE)


def main():
    img = Image.new("RGB", (1280, 640), NIGHT)
    d = ImageDraw.Draw(img)
    mark(d, 110, 200, 220)
    d.text((380, 215), "CaseComplete", font=font("segoeuib.ttf", 92), fill=WHITE)
    d.text((386, 330), "Just type.", font=font("segoeuib.ttf", 46), fill=WHITE)
    d.text((386, 395), "Fixes your typos, finishes your sentences, learns how you write.", font=font("segoeui.ttf", 30), fill=GHOST)
    d.text((386, 450), "Windows  ·  100% local  ·  open source", font=font("segoeuisb.ttf", 26), fill=BLUE)
    out = HERE / "social-preview.png"
    img.save(out, optimize=True)
    print(out)


if __name__ == "__main__":
    main()
