"""Generate the WrongIndex desktop icons (deterministic; no external assets).

Run: python scripts/desktop/make_icons.py
"""
import os
from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "..", "..", "crates", "wrongindex-gpui", "icons")


def draw(size: int) -> Image.Image:
    s = size
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    r = max(2, s // 6)
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=r, fill=(24, 28, 38, 255))
    # Three index rows with a highlighted "current" marker.
    pad = s * 0.2
    h = s * 0.1
    for i, (w, col) in enumerate([(0.6, (120, 200, 255)), (0.45, (170, 180, 200)), (0.55, (170, 180, 200))]):
        y = pad + i * s * 0.2
        d.rounded_rectangle([pad, y, pad + s * w, y + h], radius=h / 2, fill=col + (255,))
    c = s * 0.72
    rr = s * 0.12
    d.ellipse([c - rr, c - rr, c + rr, c + rr], fill=(90, 220, 140, 255))
    return img


os.makedirs(OUT, exist_ok=True)
big = draw(256)
big.save(os.path.join(OUT, "icon.png"))
draw(32).save(os.path.join(OUT, "32x32.png"))
draw(128).save(os.path.join(OUT, "128x128.png"))
big.save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print("icons written to", os.path.abspath(OUT))
