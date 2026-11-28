# Vendored libraries

Served by the console from its own paths; no CDN (docs/owner.md §6).

| Package | Version | Licence | Source |
| --- | --- | --- | --- |
| ag-grid-community | 36.3.0 | MIT (`ag-grid/LICENSE.txt`) | npm `ag-grid-community-36.3.0.tgz`, `sha512-0JRaIwE82TuOcx+upA3OnTyhDAIUDqrMxLoBib6yVON0SyfICdpOnwD4QdiT0A1lgrC8SkzLBFRst73bTREF6Q==` |

`ag-grid/` holds, unchanged, `dist/ag-grid-community.min.noStyle.js` (the grid without
the styles it would inject, which the CSP refuses), `styles/ag-grid.min.css`,
`styles/ag-theme-quartz.min.css` and `LICENSE.txt`. The one change: each `data:` URL in
the two stylesheets is written out as `data-<sha256 prefix>.<ext>` beside them, so
`img-src` and `font-src` stay `'self'`. Community features only; no Enterprise module
or licence key.

To move to another version:

```
python3 -I update-ag-grid.py VERSION SHA512 SCRATCH_DIR
```

with `SHA512` the registry's `dist.integrity`. Then set `AG_GRID_VERSION` and the file
list `AG_GRID_FILES` in `crates/console/src/lib.rs`, and this table.
