"""Рисует значок VramDisk (pycairo) и собирает icon.ico, icon_64.rgba (окно)
и icon_128.rgba (значок в шапке).

Значок: скруглённый квадрат с градиентом #3B82F6 → #7C3AED и белый «чип»
с выводами.
"""
import math, os
import cairo
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def rounded(ctx, x, y, w, h, r):
    ctx.new_sub_path()
    ctx.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    ctx.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    ctx.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    ctx.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    ctx.close_path()


def draw(size):
    surf = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    ctx = cairo.Context(surf)
    small = size <= 24
    ctx.scale(size / 256, size / 256)
    # фон
    m = 4 if small else 8
    rounded(ctx, m, m, 256 - 2 * m, 256 - 2 * m, 60)
    g = cairo.LinearGradient(0, 0, 256, 256)
    g.add_color_stop_rgb(0, 0x3B / 255, 0x82 / 255, 0xF6 / 255)
    g.add_color_stop_rgb(1, 0x7C / 255, 0x3A / 255, 0xED / 255)
    ctx.set_source(g)
    ctx.fill_preserve()
    sh = cairo.LinearGradient(0, 0, 0, 256)
    sh.add_color_stop_rgba(0, 1, 1, 1, 0.18)
    sh.add_color_stop_rgba(0.5, 1, 1, 1, 0)
    ctx.set_source(sh)
    ctx.fill()
    # чип
    ctx.set_source_rgb(1, 1, 1)
    if small:
        # на мелких размерах — меньше деталей, толще линии
        lw, a, b, pins = 26, 62, 194, [104, 152]
        plen = 30
    else:
        lw, a, b, pins = 17, 66, 190, [97, 128, 159]
        plen = 29
    ctx.set_line_width(lw)
    rounded(ctx, a, a, b - a, b - a, 19 if not small else 22)
    ctx.stroke()
    for p in pins:
        for (x0, y0, x1, y1) in [
            (p, a - plen, p, a), (p, b, p, b + plen),
            (a - plen, p, a, p), (b, p, b + plen, p),
        ]:
            ctx.move_to(x0, y0)
            ctx.line_to(x1, y1)
    ctx.stroke()
    ctx.set_source_rgba(1, 1, 1, 0.92)
    c = 96 if not small else 100
    rounded(ctx, c, c, 256 - 2 * c, 256 - 2 * c, 10)
    ctx.fill()
    surf.flush()
    img = Image.frombuffer("RGBA", (size, size), bytes(surf.get_data()), "raw", "BGRa", surf.get_stride(), 1)
    return img.copy()


def write_ico(path, images):
    """ICO: до 48 px — 32-битные BMP (лучше для мелких значков), крупнее — PNG."""
    import io, struct
    blobs = []
    for im in images:
        w, h = im.size
        if w <= 48:
            px = im.convert("RGBA")
            rows = []
            for y in range(h - 1, -1, -1):
                row = bytearray()
                for x in range(w):
                    r, g, b, a = px.getpixel((x, y))
                    row += bytes((b, g, r, a))
                rows.append(bytes(row))
            mask_row = ((w + 31) // 32) * 4
            data = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, w * h * 4 + mask_row * h, 0, 0, 0, 0)
            data += b"".join(rows) + bytes(mask_row * h)
        else:
            buf = io.BytesIO()
            im.save(buf, "PNG", optimize=True)
            data = buf.getvalue()
        blobs.append((w, h, data))
    out = struct.pack("<HHH", 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    for w, h, data in blobs:
        out += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    out += b"".join(d for _, _, d in blobs)
    open(path, "wb").write(out)


os.makedirs(os.path.join(HERE, "png"), exist_ok=True)
files = []
for s in SIZES:
    f = os.path.join(HERE, "png", f"icon-{s}.png")
    draw(s).save(f)
    files.append(f)
for s in (64, 128):
    raw = draw(s).tobytes()
    assert len(raw) == s * s * 4
    open(os.path.join(HERE, f"icon_{s}.rgba"), "wb").write(raw)
write_ico(os.path.join(HERE, "icon.ico"), [draw(s) for s in SIZES])
print("ok", len(files), "png; rgba", len(raw))
