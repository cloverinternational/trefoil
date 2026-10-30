# Trefoil brand

<p align="center"><img src="../assets/brand/lockup-light.svg" alt="trefoil" width="360"></p>

## The mark

Three equal lobes, each bitten by its clockwise neighbour, chase around one point.
The lobes are the providers; the three channels taper into a single centre, the one
endpoint every request passes through; the turning motion is routing and failover
that never stops. No lobe leads.

It is built only from circles and boolean cuts (`build.py`):

| | Full (`mark.svg`, above 24px) | Small (`mark-small.svg`, 24px and below) |
|---|---|---|
| Lobe centres | radius R = 13.5 around the hub, at −90°, 30°, 150° | R = 14 |
| Lobe radius | r = 11.5 | r = 11.25 |
| Bite | disc r + g, g = 4.5, centred on the next lobe pulled s = 1.5 toward the hub | g = 5.75, s = 0.75 |
| Fit | longest side 54 of the 64 grid, area centroid at x = 32, optically centred | longest side 60 |
| Narrowest channel | 1.53px at 16px | 1.98px at 16px |

Every file is a single filled path: no strokes, masks, transforms or live fonts.

## Color

One OKLCH hue (156, a cool jade), three lightnesses.

| Token | Hex | OKLCH | Use | Contrast |
|---|---|---|---|---|
| Mark | `#1d9f61` | 0.62 0.142 156 | the mark on light grounds, single-color default | 3.25:1 on `#fafafa` (graphic) |
| Mark, dark | `#35b271` | 0.68 0.145 156 | the mark on dark grounds | 7.31:1 on `#0a0a0a` |
| UI light | `#187647` | 0.50 0.112 156 | primary, links, success text on light | 5.41:1 on `#fafafa`; white label 5.64:1 |
| UI dark | `#60d291` | 0.78 0.140 156 | primary, links, success text on dark | 10.49:1 on `#0a0a0a` |

Ink is `#0a0a0a` on light and `#fafafa` on dark. Never set body text in the mark green.
Success shares the brand green, so success and error badges always carry an icon and
a word. Destructive is `#b91c1c` (light) and `#f87171` (dark), chosen so it stays
distinct from the brand green for protan and deutan vision (ΔE2000 ≥ 12.8 in light;
7.5 for deuteranopes in dark, where labels and icons carry the meaning).

## Wordmark and lockup

Lowercase `trefoil` in Geist SemiBold (600), tracking −0.018 em, with optical pair
kerning, converted to outlines. In the lockup the wordmark x-height is 0.44 of the
mark height, the gap is 0.28 and clear space on every side is 0.25 of the mark height.
Minimum lockup width 96px. In running text and UI labels the product name is
"Trefoil".

## Files

`docs/assets/brand/`: `mark.svg`, `mark-small.svg` (black), `mark-color.svg`,
`mark-small-color.svg`, `mark-color-dark.svg`, `mark-white.svg`, `lockup-light.svg`,
`lockup-dark.svg`, `favicon.svg` (switches to the dark-ground green with the browser
theme) and `paths.json` (path data used by the README banner and the admin UI's
`TrefoilMark`). Rasters: `frontend/public/favicon.ico` (16/32/48),
`frontend/public/apple-touch-icon.png`, `docs/assets/social-preview.png` (upload it as
the GitHub social preview under the repository settings).

## Don't

Rotate, recolor outside the palette, outline, add gradients, glows or shadows, set the
wordmark in a live font, or use the full mark below 24px.

## Rebuild

```sh
pip install skia-pathops fonttools
curl -fsSL -o 'docs/brand/fonts/Geist[wght].ttf' --create-dirs \
  'https://github.com/google/fonts/raw/main/ofl/geist/Geist%5Bwght%5D.ttf'
python3 docs/brand/build.py
```
