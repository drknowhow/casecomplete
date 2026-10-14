"""Writes installer/casecomplete.ico: a blue rounded square with a white C, 16/32/48/256 px."""
import math
import struct
from pathlib import Path

BLUE = (0x3A, 0x7B, 0xD5)
SIZES = [16, 32, 48, 256]


def pixel(x, y, n):
    """BGRA for pixel (x, y) in an n×n icon, premultiplied not needed for ICO."""
    s = x / n
    t = y / n
    r = 0.22
    # rounded square
    inside = True
    for cx, cy in ((r, r), (1 - r, r), (r, 1 - r), (1 - r, 1 - r)):
        dx, dy = s - cx, t - cy
        corner = (s < r or s > 1 - r) and (t < r or t > 1 - r)
        if corner and (dx * dx + dy * dy) > r * r:
            inside = False
    if not inside:
        return (0, 0, 0, 0)
    # white C: ring centred, with an opening on the right
    dx, dy = s - 0.5, t - 0.5
    d = math.hypot(dx, dy)
    ang = math.atan2(dy, dx)
    ring = 0.19 <= d <= 0.33
    opening = abs(ang) < 0.62
    if ring and not opening:
        return (255, 255, 255, 255)
    return (BLUE[2], BLUE[1], BLUE[0], 255)


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


def main():
    images = [(n, bmp_image(n)) for n in SIZES]
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    body = bytearray()
    for n, data in images:
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset + len(body))
        body += data
    path = Path(__file__).with_name("casecomplete.ico")
    path.write_bytes(bytes(out) + bytes(body))
    print(path, len(out) + len(body), "bytes")


if __name__ == "__main__":
    main()
