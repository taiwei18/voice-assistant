"""Regenerate the app icon and runtime RGBA assets. Requires Pillow."""
from pathlib import Path
from PIL import Image, ImageDraw

assets = Path(__file__).resolve().parents[1] / "assets"
scale = 4
im = Image.new("RGBA", (256 * scale, 256 * scale))
draw = ImageDraw.Draw(im)
def capsule(box, radius, color):
    draw.rounded_rectangle(tuple(int(v * scale) for v in box), radius=radius * scale, fill=color)
capsule((8, 8, 248, 248), 64, "#101e27")
bars = [(56, 104, 80, 152, "#53dbc4"), (96, 76, 120, 180, "#c6fff0"),
        (136, 56, 160, 200, "#53dbc4"), (176, 92, 200, 164, "#53dbc4")]
for x1, y1, x2, y2, color in bars:
    capsule((x1, y1, x2, y2), 12, color)
im = im.resize((256, 256), Image.Resampling.LANCZOS)
im.save(assets / "app.png")
im.save(assets / "app.ico", sizes=[(16,16),(20,20),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])
(assets / "app-32.rgba").write_bytes(im.resize((32,32), Image.Resampling.LANCZOS).tobytes())
(assets / "app-64.rgba").write_bytes(im.resize((64,64), Image.Resampling.LANCZOS).tobytes())
svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256"><rect x="8" y="8" width="240" height="240" rx="64" fill="#101e27"/>'
for x1,y1,x2,y2,color in bars:
    svg += f'<rect x="{x1}" y="{y1}" width="{x2-x1}" height="{y2-y1}" rx="12" fill="{color}"/>'
(assets / "app.svg").write_text(svg + '</svg>\n', encoding="utf-8")
