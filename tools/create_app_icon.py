"""Draw the independent soundbar icon; requires Pillow. No vendor artwork."""

from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SCALE = 4
SIZE = 512
image = Image.new("RGBA", (SIZE * SCALE, SIZE * SCALE))
draw = ImageDraw.Draw(image)
svg = []


def box(values):
    return tuple(round(v * SCALE) for v in values)


def rounded_rect(x, y, w, h, radius, fill):
    draw.rounded_rectangle(box((x, y, x + w, y + h)), radius * SCALE, fill)
    svg.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{radius}" fill="{fill}"/>')


def circle(x, y, radius, fill, stroke=None, width=0):
    # Pillow draws outlines inward; draw a centred stroke to match the SVG.
    if stroke:
        circle(x, y, radius + width / 2, stroke)
        circle(x, y, radius - width / 2, fill)
        return
    draw.ellipse(box((x - radius, y - radius, x + radius, y + radius)), fill)
    svg.append(f'<circle cx="{x}" cy="{y}" r="{radius}" fill="{fill}"/>')


def line(x1, y1, x2, y2, fill, width):
    draw.line(box((x1, y1, x2, y2)), fill, width * SCALE)
    for x, y in [(x1, y1), (x2, y2)]:
        draw.ellipse(box((x - width / 2, y - width / 2, x + width / 2, y + width / 2)), fill)
    svg.append(f'<path d="M{x1} {y1} L{x2} {y2}" stroke="{fill}" stroke-width="{width}" stroke-linecap="round"/>')


def arc(x, y, radius, fill, width):
    draw.arc(box((x - radius, y - radius, x + radius, y + radius)), 180, 360,
             fill, width * SCALE)
    # Match the centred radius of the inward Pillow stroke.
    r = radius - width / 2
    svg.append(f'<path d="M{x-r} {y} A{r} {r} 0 0 1 {x+r} {y}" fill="none" stroke="{fill}" stroke-width="{width}"/>')


# A simple silhouette keeps the symbol readable at 16 and 24 pixels.
rounded_rect(20, 20, 472, 472, 112, "#111E35")
arc(256, 222, 132, "#657CF5", 16)
arc(256, 222, 94, "#57E5D1", 16)
rounded_rect(76, 260, 360, 142, 62, "#080F20")
rounded_rect(76, 246, 360, 142, 62, "#EAF4FF")
circle(155, 317, 43, "#172B46")
circle(357, 317, 43, "#172B46")
circle(155, 317, 27, "#172B46", "#30CBBB", 8)
circle(357, 317, 27, "#172B46", "#7B83F7", 8)
line(237, 307, 237, 327, "#34506F", 8)
line(256, 295, 256, 339, "#34506F", 8)
line(275, 307, 275, 327, "#34506F", 8)

ASSETS.mkdir(exist_ok=True)
(ASSETS / "app-icon.svg").write_text(
    '<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">\n'
    '  <title>Halo Soundbar independent app icon</title>\n  '
    + '\n  '.join(svg) + '\n</svg>\n', encoding="utf-8")
image = image.resize((512, 512), Image.Resampling.LANCZOS)
image.save(ASSETS / "app-icon.png")
image.save(ASSETS / "app.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
print("Wrote assets/app-icon.svg, app-icon.png and app.ico")
