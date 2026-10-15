#!/usr/bin/env python3
"""Genera assets/icon.ico (16, 24, 32, 48, 64, 256 px) con lo stesso disegno
dell'icona della finestra: tre barre verde/ambra/rossa su fondo scuro arrotondato.
Solo libreria standard: i PNG sono scritti a mano con zlib."""
import struct, zlib
from pathlib import Path

BG = (32, 36, 44)
BARS = [(0.31, (46, 160, 67)), (0.50, (222, 158, 36)), (0.69, (222, 64, 64))]


def pixel(x, y, n):
    """Colore RGBA del pixel (x, y) per un'icona n×n, con bordi arrotondati."""
    u, v = (x + 0.5) / n, (y + 0.5) / n
    m, r = 0.06, 0.16  # margine e raggio dello sfondo, in frazione del lato
    cx = min(max(u, m + r), 1 - m - r)
    cy = min(max(v, m + r), 1 - m - r)
    if (u - cx) ** 2 + (v - cy) ** 2 > r * r:
        return (0, 0, 0, 0)
    for k, (h, col) in enumerate(BARS):
        x0 = 0.19 + k * 0.235
        if x0 <= u < x0 + 0.16 and 0.84 - h <= v < 0.84:
            return col + (255,)
    return BG + (255,)


def png(n):
    rows = b''.join(b'\x00' + b''.join(bytes(pixel(x, y, n)) for x in range(n)) for y in range(n))
    def chunk(tag, data):
        return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data) & 0xFFFFFFFF)
    ihdr = struct.pack('>IIBBBBB', n, n, 8, 6, 0, 0, 0)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(rows, 9)) + chunk(b'IEND', b'')


sizes = [16, 24, 32, 48, 64, 256]
images = [png(n) for n in sizes]
header = struct.pack('<HHH', 0, 1, len(sizes))
offset = 6 + 16 * len(sizes)
entries = b''
for n, img in zip(sizes, images):
    entries += struct.pack('<BBBBHHII', n % 256, n % 256, 0, 0, 1, 32, len(img), offset)
    offset += len(img)
out = Path(__file__).with_name('icon.ico')
out.write_bytes(header + entries + b''.join(images))
print(f'scritto {out} ({out.stat().st_size} byte)')
