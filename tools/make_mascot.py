"""Cuts the mascot sticker out of its background and writes every size the app needs.

  python3 tools/make_mascot.py <source.png>

Outputs (paths relative to the repo):
  ui/mascot.png                         560 px, eyes removed (the app draws the eyes per mood)
  src-tauri/assets/mascot-160.rgba      160x160 raw straight RGBA, eyes removed (taskbar + tray)
  src-tauri/icons/*                     app icon with eyes (ico + png)
"""
import os, struct, sys
from collections import deque
sys.path.insert(0, os.path.dirname(__file__))
import pngio

ROOT = os.path.join(os.path.dirname(__file__), '..')
EYES = [(335, 437, 418, 525), (851, 437, 934, 525)]  # x0, y0, x1, y1 in the source image
CROP = 1120

w, h, px = pngio.read(sys.argv[1])

# 1) Background = everything reachable from the border without crossing the white outline.
outside = bytearray(w * h)
q = deque()
def is_bg(i):
    o = i * 4
    return min(px[o], px[o + 1], px[o + 2]) < 238
for x in range(w):
    for y in (0, h - 1):
        q.append(y * w + x)
for y in range(h):
    for x in (0, w - 1):
        q.append(y * w + x)
while q:
    i = q.popleft()
    if outside[i] or not is_bg(i):
        continue
    outside[i] = 1
    x, y = i % w, i // w
    if x > 0: q.append(i - 1)
    if x < w - 1: q.append(i + 1)
    if y > 0: q.append(i - w)
    if y < h - 1: q.append(i + w)
for i in range(w * h):
    if outside[i]:
        px[i * 4 + 3] = 0

# 2) Remove the eyes: refill each row of the eye box between the skin colours either side.
def lift_eyes(buf):
    for x0, y0, x1, y1 in EYES:
        x0 -= 4; x1 += 4; y0 -= 4; y1 += 4
        for y in range(y0, y1 + 1):
            a = buf[(y * w + x0 - 3) * 4:(y * w + x0 - 3) * 4 + 3]
            b = buf[(y * w + x1 + 3) * 4:(y * w + x1 + 3) * 4 + 3]
            for x in range(x0, x1 + 1):
                t = (x - x0) / (x1 - x0)
                o = (y * w + x) * 4
                for k in range(3):
                    buf[o + k] = round(a[k] + (b[k] - a[k]) * t)
eyeless = bytearray(px)
lift_eyes(eyeless)

# 3) Square crop around the sticker.
xs = [i % w for i in range(w * h) if px[i * 4 + 3]]
ys = [i // w for i in range(w * h) if px[i * 4 + 3]]
cx, cy = (min(xs) + max(xs)) // 2, (min(ys) + max(ys)) // 2
ox, oy = cx - CROP // 2, cy - CROP // 2
def crop(buf):
    out = bytearray(CROP * CROP * 4)
    for y in range(CROP):
        sy = y + oy
        if not 0 <= sy < h: continue
        for x in range(CROP):
            sx = x + ox
            if 0 <= sx < w:
                s = (sy * w + sx) * 4; d = (y * CROP + x) * 4
                out[d:d + 4] = buf[s:s + 4]
    return out
full_c, eyeless_c = crop(px), crop(eyeless)

# 4) Area-weighted (box) resize in premultiplied space, separable.
def resize(buf, n0, n):
    def weights(i):
        a, b = i * n0 / n, (i + 1) * n0 / n
        return [(j, min(b, j + 1) - max(a, j)) for j in range(int(a), min(n0, int(b) + 1)) if min(b, j + 1) > max(a, j)]
    W = [weights(i) for i in range(n)]
    pm = [0.0] * (n0 * n0 * 4)
    for i in range(n0 * n0):
        al = buf[i * 4 + 3] / 255
        pm[i * 4:i * 4 + 4] = [buf[i * 4] * al, buf[i * 4 + 1] * al, buf[i * 4 + 2] * al, al]
    tmp = [0.0] * (n0 * n * 4)
    for y in range(n0):
        for x in range(n):
            acc = [0.0] * 4; tot = 0
            for j, wt in W[x]:
                s = (y * n0 + j) * 4
                for k in range(4): acc[k] += pm[s + k] * wt
                tot += wt
            d = (y * n + x) * 4
            for k in range(4): tmp[d + k] = acc[k] / tot
    out = bytearray(n * n * 4)
    for y in range(n):
        for x in range(n):
            acc = [0.0] * 4; tot = 0
            for j, wt in W[y]:
                s = (j * n + x) * 4
                for k in range(4): acc[k] += tmp[s + k] * wt
                tot += wt
            a = acc[3] / tot
            d = (y * n + x) * 4
            if a > 0.002:
                for k in range(3): out[d + k] = max(0, min(255, round(acc[k] / tot / a)))
            out[d + 3] = round(a * 255)
    return out

pngio.write(os.path.join(ROOT, 'ui', 'mascot.png'), 560, 560, resize(eyeless_c, CROP, 560))
os.makedirs(os.path.join(ROOT, 'src-tauri', 'assets'), exist_ok=True)
open(os.path.join(ROOT, 'src-tauri', 'assets', 'mascot-160.rgba'), 'wb').write(bytes(resize(eyeless_c, CROP, 160)))

icons = os.path.join(ROOT, 'src-tauri', 'icons')
big = resize(full_c, CROP, 256)
imgs = {}
for n in (16, 24, 32, 48, 64, 128, 256):
    buf = big if n == 256 else resize(big, 256, n)
    path = os.path.join(icons, f'_{n}.png'); pngio.write(path, n, n, buf); imgs[n] = open(path, 'rb').read(); os.remove(path)
for n, name in ((32, '32x32.png'), (128, '128x128.png'), (256, 'icon.png')):
    open(os.path.join(icons, name), 'wb').write(imgs[n])
sizes = (16, 24, 32, 48, 64, 256)
head = struct.pack('<HHH', 0, 1, len(sizes)); off = 6 + 16 * len(sizes); dirs = b''; blobs = b''
for n in sizes:
    d = imgs[n]; dirs += struct.pack('<BBBBHHII', n % 256, n % 256, 0, 0, 1, 32, len(d), off + len(blobs)); blobs += d
open(os.path.join(icons, 'icon.ico'), 'wb').write(head + dirs + blobs)

for x0, y0, x1, y1 in EYES:
    f = lambda v, o: round((v - o) / CROP * 100, 2)
    print('eye (0-100 space): x', f(x0, ox), '..', f(x1 + 1, ox), ' y', f(y0, oy), '..', f(y1 + 1, oy))
print('crop origin', ox, oy)
