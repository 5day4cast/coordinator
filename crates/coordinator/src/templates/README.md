# Templates & Frontend Assets

Pages are rendered on the server with [Maud](https://maud.lambda.xyz/) and
updated with [htmx](https://htmx.org/). JavaScript is kept for what only the
browser can do: the WASM wallet (keys, signing, payment), the htmx hook that
signs account requests, and a few small page behaviours.

## Structure

```
templates/
├── shared/        # scripts used across pages (auth hook, WASM loader, page helpers)
├── layouts/       # page shells (base/ has the public layout, its script and styles)
├── components/    # navbar, dialogs
├── fragments/     # htmx partials; a folder when it has its own .js/.css
├── pages/         # full pages (competitions, entries, payouts)
├── admin/         # operator dashboard
└── static/        # global styles.css, vendored htmx, the map outline
```

A component's `.css` and `.js` sit beside the Rust that renders it.

## Build

`build.rs` bundles everything into `OUT_DIR`; nothing is written to the source
tree:

| Bundle | Contents |
|--------|----------|
| `app.js` | every `.js` under `shared`, `components`, `fragments`, `pages`, `layouts`; `base.js` last |
| `admin.js` | every `.js` under `admin` |
| `styles.css` | `static/styles.css`, then every other `.css`, minified with lightningcss |
| `htmx.js`, `usa-map.svg` | copied from `static/` |

Scripts are minified with oxc, each parsed as a classic script so top-level
names stay global; a script that does not parse fails the build. Each file is
embedded with `include_bytes!`,
with a gzipped copy, at `/assets/<name>.<hash>.<ext>` and served with a
one-year immutable cache. Templates link them through the constants in
`templates::assets` (`APP_JS.url`, `STYLES_CSS.url`, …).

The WASM package is built separately by `wasm-pack` into the UI directory
(`[ui_settings].ui_dir`, `crates/public_ui` in development) and served from
`/ui/pkg/`. Pages request it with `?v=<hash>` computed at startup, which is
also cached for a year; `shared/wasm.js` loads it on first use (opening the
log-in or sign-up dialog), not on every page view.

## Conventions

- Handlers answer htmx requests (`HX-Request`) with content only and direct
  visits with the whole page, so every URL works when reloaded or shared.
- Account pages (`/entries`, `/payouts`) are signed with NIP-98 by
  `shared/htmx_auth.js`; opened directly they show a log-in prompt that loads
  the page once the visitor logs in (the `fw:login` event).
- Times are rendered as `<time datetime=… data-local>` in UTC and shown in the
  reader's time zone by `shared/page.js`.
- Prefer attributes (`hx-get`, `data-open-modal`, `data-copy`) and delegated
  listeners over inline `onclick` and per-element setup.
