"""Build: uv run --with fonttools tools/compact_font.py INPUT.ttf OUTPUT.ttf.

Input: qintmb/herdr-icon-agent-ui @ 8fb40b8951b5f7331815949d6c2a5bb3a06ef4c6
       dist/HerdrAgentIconsMax-Regular.ttf
Uniformly fit each outline inside a 520x620 box on a 600-unit advance.
The distinct family avoids changing other plugins' large icon face.
"""
import sys
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.transformPen import TransformPen

source, output = map(Path, sys.argv[1:])
font = TTFont(source, recalcTimestamp=False)
outlines = font.getGlyphSet()
replacements = {}
for name in font.getGlyphOrder():
    glyph = font['glyf'][name]
    if not glyph.numberOfContours:
        continue
    glyph.recalcBounds(font['glyf'])
    width, height = glyph.xMax - glyph.xMin, glyph.yMax - glyph.yMin
    scale = min(520 / width, 620 / height, 1)
    x = 300 - (glyph.xMin + glyph.xMax) * scale / 2
    y = 365 - (glyph.yMin + glyph.yMax) * scale / 2
    pen = TTGlyphPen(outlines)
    glyph.draw(TransformPen(pen, (scale, 0, 0, scale, x, y)), font['glyf'])
    replacements[name] = pen.glyph()
for name, glyph in replacements.items():
    font['glyf'][name] = glyph
    glyph.recalcBounds(font['glyf'])
    font['hmtx'][name] = (600, glyph.xMin)
    assert 39 <= glyph.xMin <= glyph.xMax <= 561
    assert glyph.yMax - glyph.yMin <= 621
for record in font['name'].names:
    if record.nameID in (1, 4, 6, 16):
        name = 'HerdrAgentIconsCompact-Regular' if record.nameID == 6 else 'Herdr Agent Icons Compact'
        record.string = name.encode(record.getEncoding())
    elif record.nameID == 3:
        record.string = 'herdr-project-sidebar:compact-1'.encode(record.getEncoding())
output.parent.mkdir(parents=True, exist_ok=True)
font.save(output)
print(f'{len(replacements)} compact glyphs, 520x620 max, advance 600: {output}')
