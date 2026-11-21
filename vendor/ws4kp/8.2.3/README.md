# WeatherStar 4000+ 8.2.3

The static build of WeatherStar 4000+ (<https://github.com/netbymatt/ws4kp>, MIT, see
`LICENSE`), the lobby's regional forecast. It is served from `/weatherstar/` with its own CSP
(`api/public_headers.rs`) and framed by the lobby.

Built from commit `1af63f9` (2026-11-12) with `npm ci && npm run build`, then trimmed:

- left out `music/`, `playlist.json`, `images/maps/radar-conus/`, source maps,
  `scripts/custom.sample.js` and `readme.txt`; the lobby shows no radar and plays no music;
- `index.html`'s inline `<script>const OVERRIDES = {};</script>` moved to
  `scripts/overrides.js`, and `style="display:none"` on `#version` replaced by `hidden`, so the
  page runs without inline script or style;
- added `scripts/custom.js`, which WeatherStar loads itself, to tour the country's regions, an
  empty `scripts/custom.mjs` and an empty `playlist.json`, so its start-up finds every file.

The Star4000 fonts and the icons come with the project, which credits TWCClassics
(<https://twcclassics.com/>) for them.
