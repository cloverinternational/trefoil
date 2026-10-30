"""Trefoil production mark — build script.

Run:  python3 docs/brand/build.py --font path/to/Geist[wght].ttf
      (needs `pip install skia-pathops fonttools`; Geist is SIL OFL 1.1,
      https://github.com/google/fonts/tree/main/ofl/geist)
Writes the marks, lockups, favicon.svg and paths.json to docs/assets/brand/.
Every shape comes from the construction below; nothing is drawn by hand.

CONSTRUCTION (units of the 64x64 master grid, before final fit-to-frame)
  Hub   O = (0, 0). Three lobe centres C0..C2 on a circle of radius R around O,
        at -90, 30, 150 degrees (C0 points straight up: a deliberate 0/120/240 stance).
  Lobe  disc of radius r around Ci.
  Bite  lobe i is cut by its clockwise neighbour j = i+1: subtract a disc of radius
        r + g whose centre is Cj pulled toward the hub by s (eccentric bite).
        The pull makes each channel taper: narrowest at the outer mouth (g - s*cos)
        and opening into the hub, so the three routes visibly drain into ONE point.
  Hub   subtract a disc of radius h at O so the junction is a clean round endpoint.
  Fit   scale so the silhouette spans `span` units, then place the AREA CENTROID
        (the rotation centre) at x=32, and vertically split the difference between the
        centroid and the bbox centre (optical centring for a triangular silhouette).
"""
import math, os, sys
import pathops
from pathops import Path, op, PathOp

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, '..', 'assets', 'brand')
K = 0.5522847498

# ---------------------------------------------------------------- parameters
FULL  = dict(R=13.5, r=11.5, g=4.5, s=1.5, h=0.0, span=54.0)   # >= 24px
SMALL = dict(R=14.0, r=11.25, g=5.75, s=0.75, h=0.0, span=60.0)  # 16-20px cut, channels kept open under blur

# ---------------------------------------------------------------- geometry
def circle(cx, cy, r):
    p = Path(); pen = p.getPen()
    pen.moveTo((cx + r, cy))
    pen.curveTo((cx + r, cy + K*r), (cx + K*r, cy + r), (cx, cy + r))
    pen.curveTo((cx - K*r, cy + r), (cx - r, cy + K*r), (cx - r, cy))
    pen.curveTo((cx - r, cy - K*r), (cx - K*r, cy - r), (cx, cy - r))
    pen.curveTo((cx + K*r, cy - r), (cx + r, cy - K*r), (cx + r, cy))
    pen.closePath(); return p

def U(*ps):
    out = ps[0]
    for q in ps[1:]: out = op(out, q, PathOp.UNION)
    return out
def D(a, b): return op(a, b, PathOp.DIFFERENCE)

def transform(p, a, b, c, d, e, f):
    out = Path(); pen = out.getPen()
    T = lambda pt: (a*pt[0] + c*pt[1] + e, b*pt[0] + d*pt[1] + f)
    for verb, pts in p.segments:
        if verb == 'moveTo': pen.moveTo(T(pts[0]))
        elif verb == 'lineTo': pen.lineTo(T(pts[0]))
        elif verb == 'qCurveTo': pen.qCurveTo(*[T(x) for x in pts])
        elif verb == 'curveTo': pen.curveTo(*[T(x) for x in pts])
        elif verb == 'closePath': pen.closePath()
        elif verb == 'endPath': pen.endPath()
    return out

def flatten(p, n=24):
    """Polygons (list of point lists) approximating the path, for area/centroid."""
    polys, cur, last = [], [], None
    for verb, pts in p.segments:
        if verb == 'moveTo':
            cur = [pts[0]]; last = pts[0]
        elif verb == 'lineTo':
            cur.append(pts[0]); last = pts[0]
        elif verb == 'curveTo':
            p0, (p1, p2, p3) = last, pts
            for k in range(1, n+1):
                t = k/n; u = 1-t
                cur.append((u**3*p0[0] + 3*u*u*t*p1[0] + 3*u*t*t*p2[0] + t**3*p3[0],
                            u**3*p0[1] + 3*u*u*t*p1[1] + 3*u*t*t*p2[1] + t**3*p3[1]))
            last = p3
        elif verb == 'qCurveTo':
            # treat as a series of quads with implied on-curve points
            ctrl, end = pts[:-1], pts[-1]
            for i, c in enumerate(ctrl):
                e = end if i == len(ctrl)-1 else ((c[0]+ctrl[i+1][0])/2, (c[1]+ctrl[i+1][1])/2)
                p0 = last
                for k in range(1, n+1):
                    t = k/n; u = 1-t
                    cur.append((u*u*p0[0] + 2*u*t*c[0] + t*t*e[0], u*u*p0[1] + 2*u*t*c[1] + t*t*e[1]))
                last = e
        elif verb in ('closePath', 'endPath'):
            if cur: polys.append(cur)
            cur = []
    return polys

def area_centroid(p):
    A = cx = cy = 0.0
    for poly in flatten(p):
        for (x0, y0), (x1, y1) in zip(poly, poly[1:] + poly[:1]):
            cr = x0*y1 - x1*y0
            A += cr; cx += (x0 + x1)*cr; cy += (y0 + y1)*cr
    A /= 2
    return abs(A), cx/(6*A), cy/(6*A)

def trefoil(R, r, g, s, h, **_):
    ang = [math.radians(-90 + 120*i) for i in range(3)]
    C = [(R*math.cos(a), R*math.sin(a)) for a in ang]
    lobes = []
    for i in range(3):
        j = (i + 1) % 3
        bx, by = C[j][0] - s*math.cos(ang[j]), C[j][1] - s*math.sin(ang[j])
        lobes.append(D(circle(*C[i], r), circle(bx, by, r + g)))
    p = U(*lobes)
    if h: p = D(p, circle(0, 0, h))
    return p

def fit(p, span, frame=64.0):
    x0, y0, x1, y1 = p.bounds
    k = span / max(x1 - x0, y1 - y0)
    p = transform(p, k, 0, 0, k, 0, 0)
    _, cx, cy = area_centroid(p)
    x0, y0, x1, y1 = p.bounds
    oy = (cy + (y0 + y1)/2) / 2          # optical: halfway between centroid and bbox centre
    return transform(p, 1, 0, 0, 1, frame/2 - cx, frame/2 - oy), k

# ---------------------------------------------------------------- svg output
def fmt(v):
    s = ('%.2f' % v).rstrip('0').rstrip('.')
    return '0' if s in ('-0', '') else s

def to_d(p, dx=0.0, dy=0.0, k=1.0):
    p = Path(p); p.simplify(); p.convertConicsToQuads()
    P = lambda x, y: '%s %s' % (fmt(x*k + dx), fmt(y*k + dy))
    out = []
    for verb, pts in p.segments:
        if verb == 'moveTo': out.append('M' + P(*pts[0]))
        elif verb == 'lineTo': out.append('L' + P(*pts[0]))
        elif verb == 'curveTo': out.append('C' + ' '.join(P(*q) for q in pts))
        elif verb == 'qCurveTo':
            ctrl, end = pts[:-1], pts[-1]
            for i, c in enumerate(ctrl):
                e = end if i == len(ctrl)-1 else ((c[0]+ctrl[i+1][0])/2, (c[1]+ctrl[i+1][1])/2)
                out.append('Q' + P(*c) + ' ' + P(*e))
        elif verb == 'closePath': out.append('Z')
    return ''.join(out)

def write(name, body):
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, name), 'w') as f: f.write(body)
    print('wrote', name)

def mark_svg(p, fill):
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img" aria-label="Trefoil">'
            '<title>Trefoil</title><path fill="%s" d="%s"/></svg>\n' % (fill, to_d(p)))

def favicon_svg(p):
    """Small cut; switches to the lifted green when the browser UI is dark."""
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">'
            '<style>path{fill:%s}@media (prefers-color-scheme:dark){path{fill:%s}}</style>'
            '<path d="%s"/></svg>\n' % (MARK, MARK_DARK, to_d(p)))

# ---------------------------------------------------------------- colour
def oklch_to_srgb(L, C, H):
    a, b = C*math.cos(math.radians(H)), C*math.sin(math.radians(H))
    l_ = L + 0.3963377774*a + 0.2158037573*b
    m_ = L - 0.1055613458*a - 0.0638541728*b
    s_ = L - 0.0894841775*a - 1.2914855480*b
    l, m, s = l_**3, m_**3, s_**3
    rgb = (4.0767416621*l - 3.3077115913*m + 0.2309699292*s,
           -1.2684380046*l + 2.6097574011*m - 0.3413193965*s,
           -0.0041960863*l - 0.7034186147*m + 1.7076147010*s)
    enc = lambda c: 12.92*c if c <= 0.0031308 else 1.055*c**(1/2.4) - 0.055
    return tuple(max(0, min(255, round(enc(max(0.0, c))*255))) for c in rgb)

def hexc(rgb): return '#%02x%02x%02x' % rgb
def lum(h):
    h = h.lstrip('#'); v = [int(h[i:i+2], 16)/255 for i in (0, 2, 4)]
    lin = [c/12.92 if c <= 0.04045 else ((c + 0.055)/1.055)**2.4 for c in v]
    return 0.2126*lin[0] + 0.7152*lin[1] + 0.0722*lin[2]
def contrast(a, b):
    la, lb = sorted((lum(a), lum(b)), reverse=True)
    return (la + 0.05)/(lb + 0.05)

# Trefoil green: one OKLCH hue (156, a cool clover/jade, greener than Tailwind emerald,
# deeper than Supabase mint, cooler than Spotify), moderate chroma: crafted, not neon.
# All three values share the hue so the system reads as one colour at three lightnesses.
HUE = 156
MARK     = hexc(oklch_to_srgb(0.620, 0.142, HUE))   # the mark; >=3:1 on both grounds
UI_LIGHT = hexc(oklch_to_srgb(0.500, 0.112, HUE))   # text/buttons on #fafafa (>=4.5:1)
UI_DARK  = hexc(oklch_to_srgb(0.780, 0.140, HUE))   # text/buttons on #0a0a0a (>=4.5:1)
MARK_DARK = hexc(oklch_to_srgb(0.680, 0.145, HUE))  # the mark on dark grounds: lifted so it holds next to #fafafa type
INK_LIGHT, INK_DARK = '#0a0a0a', '#fafafa'

# ---------------------------------------------------------------- wordmark
FONT = sys.argv[sys.argv.index('--font') + 1] if '--font' in sys.argv else os.path.join(HERE, 'fonts', 'Geist[wght].ttf')
WEIGHT = 600
TEXT = 'trefoil'
TRACK = -0.018          # em, applied between letters
MANUAL_KERN = {('t', 'r'): -0.004, ('f', 'o'): -0.010, ('e', 'f'): 0.004,
               ('o', 'i'): 0.006, ('i', 'l'): 0.012}   # em, by eye after proofing

def gpos_kern(font, left, right):
    if 'GPOS' not in font: return 0
    gpos = font['GPOS'].table
    for fr in gpos.FeatureList.FeatureRecord:
        if fr.FeatureTag != 'kern': continue
        for li in fr.Feature.LookupListIndex:
            lk = gpos.LookupList.Lookup[li]
            subs = lk.SubTable
            if lk.LookupType == 9: subs = [s.ExtSubTable for s in subs]
            for st in subs:
                if getattr(st, 'LookupType', 2) != 2 or left not in st.Coverage.glyphs: continue
                if st.Format == 1:
                    ps = st.PairSet[st.Coverage.glyphs.index(left)]
                    for pv in ps.PairValueRecord:
                        if pv.SecondGlyph == right:
                            return getattr(pv.Value1, 'XAdvance', 0) or 0
                else:
                    c1 = st.ClassDef1.classDefs.get(left, 0)
                    c2 = st.ClassDef2.classDefs.get(right, 0)
                    v = st.Class1Record[c1].Class2Record[c2].Value1
                    x = getattr(v, 'XAdvance', 0) if v else 0
                    if x: return x
    return 0

def wordmark():
    from fontTools.ttLib import TTFont
    from fontTools.varLib.instancer import instantiateVariableFont
    from fontTools.pens.recordingPen import DecomposingRecordingPen as RecordingPen
    font = instantiateVariableFont(TTFont(FONT), {'wght': WEIGHT})
    upm = font['head'].unitsPerEm
    cmap = font.getBestCmap(); gs = font.getGlyphSet()
    names = [cmap[ord(ch)] for ch in TEXT]
    out = Path(); x = 0.0
    for i, (ch, gn) in enumerate(zip(TEXT, names)):
        rec = RecordingPen(gs); gs[gn].draw(rec)
        gp = Path(); pen = gp.getPen()
        for verb, args in rec.value:
            if verb == 'closePath': pen.closePath()
            elif verb == 'endPath': pen.endPath()
            else: getattr(pen, verb)(*[(a + x, -b) for a, b in args])
        out = U(out, gp) if i else gp
        x += gs[gn].width
        if i < len(TEXT) - 1:
            nxt = TEXT[i + 1]
            x += gpos_kern(font, gn, names[i + 1]) + (TRACK + MANUAL_KERN.get((ch, nxt), 0))*upm
    os2 = font['OS/2']
    return out, upm, os2.sxHeight, os2.sCapHeight

def lockup(mark_full, mark_fill, ink, name):
    """Proportions in units of the mark's height M:
       x-height of the wordmark = 0.44 M; the x-height band is centred on the mark's
       bbox centre; gap mark->wordmark = 0.28 M; clear space on every side = 0.25 M
       (about two lobe-channel widths). Minimum lockup width: 96px (mark >= 20px)."""
    word, upm, xh, cap = wordmark()
    mx0, my0, mx1, my1 = mark_full.bounds
    mh = my1 - my0
    k = (0.44*mh) / xh                     # wordmark scale
    wx0, wy0, wx1, wy1 = word.bounds
    gap = 0.28*mh
    clear = 0.25*mh
    base = (my0 + my1)/2 + xh*k/2           # x-height band centred on the mark
    tx = mx1 + gap - wx0*k
    W = (tx + wx1*k) - mx0 + 2*clear
    H = mh + 2*clear
    ox, oy = clear - mx0, clear - my0
    d_mark = to_d(mark_full, ox, oy)
    d_word = to_d(word, tx + ox, base + oy, k)
    svg = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 %s %s" role="img" aria-label="Trefoil">'
           '<title>Trefoil</title><path fill="%s" d="%s"/><path fill="%s" d="%s"/></svg>\n'
           % (fmt(W), fmt(H), mark_fill, d_mark, ink, d_word))
    write(name, svg)
    return W, H, d_mark, d_word

# ---------------------------------------------------------------- report helpers
def lobes(R, r, g, s, **_):
    ang = [math.radians(-90 + 120*i) for i in range(3)]
    C = [(R*math.cos(a), R*math.sin(a)) for a in ang]
    return [D(circle(*C[i], r), circle(C[(i+1) % 3][0] - s*math.cos(ang[(i+1) % 3]),
                                       C[(i+1) % 3][1] - s*math.sin(ang[(i+1) % 3]), r + g))
            for i in range(3)]

def min_gap(P, k):
    """Measured narrowest channel between two neighbouring lobes, in final 64-grid units."""
    L = lobes(**P)
    A = [q for poly in flatten(L[0], 60) for q in poly]
    B = [q for poly in flatten(L[1], 60) for q in poly]
    return min(math.dist(a, b) for a in A for b in B) * k

if __name__ == '__main__':
    full, kf = fit(trefoil(**FULL), FULL['span'])
    small, ks = fit(trefoil(**SMALL), SMALL['span'])
    write('mark.svg', mark_svg(full, '#000'))
    write('mark-small.svg', mark_svg(small, '#000'))
    write('mark-color.svg', mark_svg(full, MARK))
    write('mark-small-color.svg', mark_svg(small, MARK))
    write('mark-white.svg', mark_svg(full, '#fff'))
    write('mark-color-dark.svg', mark_svg(full, MARK_DARK))
    write('favicon.svg', favicon_svg(small))
    lockup(full, MARK, INK_LIGHT, 'lockup-light.svg')
    W, H, d_mark, d_word = lockup(full, MARK_DARK, INK_DARK, 'lockup-dark.svg')
    import json
    write('paths.json', json.dumps({
        'mark': to_d(full), 'small': to_d(small),
        'lockup': {'width': round(W, 2), 'height': round(H, 2), 'mark': d_mark, 'wordmark': d_word},
        'colors': {'mark': MARK, 'mark_dark': MARK_DARK, 'ui_light': UI_LIGHT, 'ui_dark': UI_DARK},
    }, indent=1) + '\n')
    for nm, P, k in (('full', FULL, kf), ('small', SMALL, ks)):
        gw = min_gap(P, k)
        print('%-5s scale %.3f  narrowest channel %.2fu = %.2fpx @16, %.2fpx @20, %.2fpx @32  bounds %s'
              % (nm, k, gw, gw/4, gw*20/64, gw/2, [round(v, 2) for v in (full if nm == 'full' else small).bounds]))
    for nm, c in (('mark', MARK), ('mark-dk', MARK_DARK), ('ui-light', UI_LIGHT), ('ui-dark', UI_DARK)):
        print('%-8s %s  on #fafafa %.2f:1  on #0a0a0a %.2f:1  on #fff %.2f:1'
              % (nm, c, contrast(c, '#fafafa'), contrast(c, '#0a0a0a'), contrast(c, '#ffffff')))
