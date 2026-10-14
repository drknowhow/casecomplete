"""Brand art for the installer, rasterised from one pixel function (no PIL).

Writes, next to this script:
  casecomplete.ico  the Tab-key mark at 16/24/32/48/64/128/256 px
  banner.bmp        493x58 WiX banner: paper, the mark at 36 px on the left
  dialog.bmp        493x312 WiX dialog: signal-blue panel 164 px wide with the
                    mark centred at 96 px, the rest paper

The mark is a signal-blue rounded keycap with an ink bottom edge and a white
tab-arrow glyph (bar, arrowhead, stop bar, round caps). Its geometry is the
120-unit reference SVG, with the keycap squared for use as an icon. The
monochrome variant is the glyph alone in one colour. Every pixel is the mean
of SAMPLES x SAMPLES sub-pixel samples, so edges are anti-aliased.

Run with: python -I installer/make_icon.py
"""
import struct
from functools import lru_cache
from pathlib import Path

BLUE = (0x3A, 0x7B, 0xD5)  # signal blue
INK = (0x17, 0x19, 0x1C)
PAPER = (0xF7, 0xF7, 0xF5)
WHITE = (0xFF, 0xFF, 0xFF)
SIZES = [16, 24, 32, 48, 64, 128, 256]
SAMPLES = 4  # per axis, so 16 samples per pixel

# Geometry, as fractions of the icon size.
MARGIN = 1 / 32  # transparent border around the keycap
RADIUS = 0.22  # corner radius, as a fraction of the keycap width
EDGE = 4 / 120  # ink bottom edge
STROKE = 0.10  # glyph stroke width
SMALL = 16  # at or below this size: no edge, heavier glyph
SMALL_STROKE = 0.125  # 2 px at 16
# Glyph segments in the reference SVG's 120-unit space; y is relative to the
# mid line (56). Bar, arrowhead (two strokes), stop bar.
GLYPH = (
    ((30, 0), (78, 0)),
    ((64, -18), (84, 0)),
    ((84, 0), (64, 18)),
    ((92, -20), (92, 20)),
)
UNITS = 120

# dialog.bmp: "glyph" puts the white monochrome glyph on the blue panel (a blue
# keycap would vanish into it); "keycap" draws the full mark there instead.
PANEL_MARK = "glyph"


def _dist2(px, py, a, b):
    """Squared distance from (px, py) to the segment a-b."""
    dx, dy = b[0] - a[0], b[1] - a[1]
    t = ((px - a[0]) * dx + (py - a[1]) * dy) / (dx * dx + dy * dy)
    t = max(0.0, min(1.0, t))
    ex, ey = a[0] + t * dx - px, a[1] + t * dy - py
    return ex * ex + ey * ey


class Mark:
    """The mark at one size. sample(u, v) takes unit-square coordinates and
    returns (r, g, b), or None where the mark is transparent."""

    def __init__(self, n, mono=None):
        self.mono = mono
        small = n <= SMALL
        self.stroke = SMALL_STROKE if small else STROKE
        if mono:
            self.edge, cy = 0.0, 0.5
        else:
            self.edge = 0.0 if small else EDGE
            cy = MARGIN + (1 - 2 * MARGIN - self.edge) / 2
        self.radius = RADIUS * (1 - 2 * MARGIN)
        self.segments = [
            ((x0 / UNITS, cy + y0 / UNITS), (x1 / UNITS, cy + y1 / UNITS))
            for (x0, y0), (x1, y1) in GLYPH
        ]
        half = self.stroke / 2
        self.half2 = half * half
        xs = [p[0] for s in self.segments for p in s]
        ys = [p[1] for s in self.segments for p in s]
        self.box = (min(xs) - half, min(ys) - half, max(xs) + half, max(ys) + half)

    def _in_keycap(self, u, v, top):
        """Inside the rounded square whose top edge is at `top`."""
        lo, hi = MARGIN, 1 - MARGIN
        bottom = top + (1 - 2 * MARGIN - self.edge)
        if not (lo <= u <= hi and top <= v <= bottom):
            return False
        r = self.radius
        dx = u - min(max(u, lo + r), hi - r)
        dy = v - min(max(v, top + r), bottom - r)
        return dx * dx + dy * dy <= r * r

    def _on_glyph(self, u, v):
        x0, y0, x1, y1 = self.box
        if not (x0 <= u <= x1 and y0 <= v <= y1):
            return False
        return any(_dist2(u, v, a, b) <= self.half2 for a, b in self.segments)

    def sample(self, u, v):
        if self.mono:
            return self.mono if self._on_glyph(u, v) else None
        if self._in_keycap(u, v, MARGIN):
            return WHITE if self._on_glyph(u, v) else BLUE
        if self.edge and self._in_keycap(u, v, MARGIN + self.edge):
            return INK
        return None


@lru_cache(maxsize=None)
def icon_mark(n):
    return Mark(n)


def coverage(mark, x, y, n):
    """Pixel (x, y) of an n x n render as straight-alpha (r, g, b, a)."""
    r = g = b = hits = 0
    for sy in range(SAMPLES):
        for sx in range(SAMPLES):
            c = mark.sample((x + (sx + 0.5) / SAMPLES) / n, (y + (sy + 0.5) / SAMPLES) / n)
            if c:
                r += c[0]
                g += c[1]
                b += c[2]
                hits += 1
    if not hits:
        return (0, 0, 0, 0)
    return (round(r / hits), round(g / hits), round(b / hits), round(255 * hits / (SAMPLES * SAMPLES)))


def pixel(x, y, n):
    """BGRA for pixel (x, y) in an n x n icon (ICO stores straight alpha)."""
    r, g, b, a = coverage(icon_mark(n), x, y, n)
    return (b, g, r, a)


def bmp_image(n):
    header = struct.pack("<IiiHHIIiiII", 40, n, n * 2, 1, 32, 0, n * n * 4, 0, 0, 0, 0)
    rows = []
    for y in range(n - 1, -1, -1):  # bottom-up
        row = bytearray()
        for x in range(n):
            row += bytes(pixel(x, y, n))
        rows.append(bytes(row))
    mask_row = b"\x00" * (((n + 31) // 32) * 4)
    return header + b"".join(rows) + mask_row * n


def write_ico(path):
    images = [(n, bmp_image(n)) for n in SIZES]
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    body = bytearray()
    for n, data in images:
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset + len(body))
        body += data
    path.write_bytes(bytes(out) + bytes(body))
    print(path, len(out) + len(body), "bytes")


def canvas(width, height, color):
    """A top-down list of rows, each a list of (r, g, b)."""
    return [[color] * width for _ in range(height)]


def paint(rows, mark, size, left, top):
    """Composite `mark` rendered at size x size onto rows at (left, top)."""
    for y in range(size):
        row = rows[top + y]
        for x in range(size):
            r, g, b, a = coverage(mark, x, y, size)
            if not a:
                continue
            br, bg, bb = row[left + x]
            k = a / 255
            row[left + x] = (round(r * k + br * (1 - k)), round(g * k + bg * (1 - k)), round(b * k + bb * (1 - k)))


def write_bmp24(path, rows):
    """24-bit uncompressed BMP from top-down rows of (r, g, b)."""
    height, width = len(rows), len(rows[0])
    pad = b"\x00" * (-width * 3 % 4)
    data = b"".join(bytes(c for r, g, b in row for c in (b, g, r)) + pad for row in reversed(rows))
    file_header = b"BM" + struct.pack("<IHHI", 14 + 40 + len(data), 0, 0, 14 + 40)
    info = struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, len(data), 2835, 2835, 0, 0)
    path.write_bytes(file_header + info + data)
    print(path, 14 + 40 + len(data), "bytes")


def write_banner(path):
    """493x58, paper, the mark at 36 px on the right, clear of the dialog title text."""
    rows = canvas(493, 58, PAPER)
    paint(rows, Mark(36), 36, 493 - 36 - 11, (58 - 36) // 2)
    write_bmp24(path, rows)


def write_dialog(path):
    """493x312, signal-blue panel 164 px wide with the mark centred at 96 px."""
    rows = canvas(493, 312, PAPER)
    for row in rows:
        row[:164] = [BLUE] * 164
    mark = Mark(96, mono=WHITE) if PANEL_MARK == "glyph" else Mark(96)
    paint(rows, mark, 96, (164 - 96) // 2, (312 - 96) // 2)
    write_bmp24(path, rows)


def main():
    here = Path(__file__).resolve().parent
    write_ico(here / "casecomplete.ico")
    write_banner(here / "banner.bmp")
    write_dialog(here / "dialog.bmp")


if __name__ == "__main__":
    main()
