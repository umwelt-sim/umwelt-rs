# umwelt logo

The mark states what the library does: a crowd of entities, one observer at the
center, and the view radius that decides which of the crowd that observer sees.
Entities inside the radius are solid, entities outside it are ghosted.

`herd` carries the sibling mark: the same crowd with no observer and no view
radius, since herd supplies load rather than watching it.

## Which file goes where

| Surface | File | Notes |
| --- | --- | --- |
| rustdoc `html_logo_url` | `umwelt-tile-small.svg` | rustdoc renders a crate logo at 48px, 35px on narrow layouts |
| rustdoc `html_favicon_url` | `favicon.svg` | follows the viewer's color scheme |
| Browser tab, non-SVG | `favicon.ico` | 16, 32 and 48px frames |
| GitHub org avatar | `png/umwelt-avatar-512.png` | square; GitHub applies its own rounding |
| GitHub repo social preview | `umwelt-social-preview.png` | 1280x640, the size GitHub documents as best display, under its 1MB limit |
| README, light background | `umwelt-mark-light.svg` | transparent |
| README, dark background | `umwelt-mark-dark.svg` | transparent |
| Slides, print, anywhere large | `png/umwelt-mark-*-1024.png` | transparent |

## Optical sizes

Three forms, because one drawing does not survive the whole range.

- **Full**, 64px and up. The complete crowd, in view and out.
- **Small**, 20 to 48px. No ghost crowd, heavier ring. Below 64px the ghosts
  stop resolving and only muddy the shape.
- **Micro**, under 20px. Ring and observer alone. At 16px an entity dot lands on
  about one pixel and reads as a smudge.

`generate.py` applies these cutoffs when it rasterizes, so `png/umwelt-icon-16.png`
is the micro form and `png/umwelt-icon-128.png` is the full one.

## Palette

| Role | Value |
| --- | --- |
| Ink, light backgrounds | `#14181D` |
| Ink, dark backgrounds | `#E6E9EC` |
| Observer | `#1FA8C4` |

Every shape carries a class: `ink` for the crowd, `ring` for the view radius,
`obs` for the observer. Styling those classes recolors a mark without touching
its geometry.

## Regenerating

```
python3 generate.py
```

Writes umwelt's assets here and herd's into the sibling checkout at
`../../../herd/assets/logo`, matching the path dependency the two repos already
use. `--skip-herd` writes only umwelt's; `--herd PATH` sends them elsewhere.

Geometry is deterministic: a fixed-seed LCG places the crowd and a relaxation
pass separates overlapping dots, so an unchanged source regenerates identical
files. Rasterizing shells out to Chrome, the only tool on hand that renders an
SVG at an exact pixel size without adding a dependency; set `CHROME` to override
the path. SVG output does not need it.

These files are not in the `include` list in `Cargo.toml`, so they do not ship
in the published crate.
