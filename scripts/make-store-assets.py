"""Draw every icon the Microsoft Store package needs from desktop/app-icon.png.

Run after changing the app icon:  python scripts/make-store-assets.py   (needs Pillow)
"""
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "desktop" / "store" / "Assets"
LISTING = ROOT / "docs" / "internal" / "store"   # local only, not in the repository

source = Image.open(ROOT / "desktop" / "app-icon.png").convert("RGBA")
tight = source.crop(source.getbbox())          # the icon without the empty margin around it


def icon(width: int, height: int, fill: float) -> Image.Image:
    """The icon centred on a transparent canvas, taking `fill` of the shorter side."""
    side = max(1, round(min(width, height) * fill))
    canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    canvas.alpha_composite(tight.resize((side, side), Image.LANCZOS), ((width - side) // 2, (height - side) // 2))
    return canvas


def save(name: str, image: Image.Image, folder: Path = ASSETS) -> None:
    folder.mkdir(parents=True, exist_ok=True)
    image.save(folder / name, "PNG", optimize=True)


# name, base width, base height, how much of it the icon fills, scales
TILES = [
    ("Square44x44Logo", 44, 44, 0.94, (100, 125, 150, 200, 400)),
    ("StoreLogo", 50, 50, 0.94, (100, 125, 150, 200, 400)),
    ("Square71x71Logo", 71, 71, 0.75, (100, 200)),
    ("Square150x150Logo", 150, 150, 0.62, (100, 200, 400)),
    ("Wide310x150Logo", 310, 150, 0.62, (100, 200)),
    ("Square310x310Logo", 310, 310, 0.55, (100, 200)),
]
for name, w, h, fill, scales in TILES:
    for s in scales:
        save(f"{name}.scale-{s}.png", icon(round(w * s / 100), round(h * s / 100), fill))

# Taskbar, Start and Explorer sizes, without a coloured plate behind them (both Windows themes).
for size in (16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256):
    image = icon(size, size, 1.0)
    for suffix in ("", "_altform-unplated", "_altform-lightunplated"):
        save(f"Square44x44Logo.targetsize-{size}{suffix}.png", image)

save("app-tile-300.png", icon(300, 300, 0.94), LISTING)   # the "app tile icon" in the Store listing
print(f"{len(list(ASSETS.glob('*.png')))} package icons, listing icon in {LISTING}")
