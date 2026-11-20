# Fonts

The public pages' typefaces, served from this site because the CSP allows fonts from
this site only. Each file is the Latin subset Google Fonts serves for the family,
unmodified, under the SIL Open Font License 1.1 (see the `OFL-*.txt` beside them).

| File | Family | Source |
| --- | --- | --- |
| `barlow-condensed-600.woff2` | Barlow Condensed 600 | <https://fonts.gstatic.com/s/barlowcondensed/v13/HTxwL3I-JCGChYJ8VI-L6OO_au7B4873z3bWuQ.woff2> |
| `barlow-condensed-700.woff2` | Barlow Condensed 700 | <https://fonts.gstatic.com/s/barlowcondensed/v13/HTxwL3I-JCGChYJ8VI-L6OO_au7B46r2z3bWuQ.woff2> |
| `barlow-condensed-700-italic.woff2` | Barlow Condensed 700 italic | <https://fonts.gstatic.com/s/barlowcondensed/v13/HTxyL3I-JCGChYJ8VI-L6OO_au7B6xTrY3Tmu4kG.woff2> |
| `public-sans.woff2` | Public Sans, variable 400 to 700 | <https://fonts.gstatic.com/s/publicsans/v21/ijwRs572Xtc6ZYQws9YVwnNGfJ4.woff2> |
| `vt323.woff2` | VT323 | <https://fonts.gstatic.com/s/vt323/v18/pxiKyp0ihIEF2isfFJU.woff2> |

sha384:

- `barlow-condensed-600.woff2`: `WxwMESDblQd661Uj1of4h0uARR5hfyFrJFa3F7sYHZTM3w7KSMdwJLflUoibIufU`
- `barlow-condensed-700.woff2`: `RP6AYjYUI9x6VaysIKYNsUG25KtBsuaouxQwHrfNsKtQuRuIeI8t8O9YM1WIAtcH`
- `barlow-condensed-700-italic.woff2`: `pUc8WergnscT1ddTIBtNaeeMolo8cQGDMTtES6kn2Y+TCtUeL2BVkyMdY3aCPA5I`
- `public-sans.woff2`: `ReOxXqTE3IA6kcLwSKsXfb/Y8K9KWtoXeE4XJWTHKjE2U62s/lcBk/W79oBiWlKr`
- `vt323.woff2`: `QJLvofbGdhBPL6ITttGSyiYZXfA3tOmIyRlsd0l0hWuY9sVMU1aNCMYgs95btLjw`

Check one with:

```sh
openssl dgst -sha384 -binary vt323.woff2 | base64
```
