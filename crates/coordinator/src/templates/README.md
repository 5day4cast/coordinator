# Templates & Frontend Assets

Server-rendered HTML using [Maud](https://maud.lambda.xyz/) with co-located
JavaScript and CSS.

## Structure

```
templates/
├── shared/        # scripts used across pages (WASM loader, auth hook, helpers)
├── layouts/       # page shells (base/ has the public layout and its script)
├── components/    # navbar, dialogs
├── fragments/     # htmx partials; a folder when it has its own .js/.css
├── pages/         # full pages (competitions, entries, payouts)
├── admin/         # operator dashboard
└── static/        # global styles.css and the map outline
```

A component's `.css` and `.js` sit beside the Rust that renders it: a
template with no script or styles is a single `foo.rs`; one with them is a
folder with `foo/mod.rs`, `foo.js` and `foo.css`.

## Build

`build.rs` bundles everything into `OUT_DIR`; nothing is written to the source
tree:

| Bundle | Contents |
|--------|----------|
| `app.js` | every `.js` under `shared`, `components`, `fragments`, `pages`, `layouts`; `base.js` last |
| `admin.js` | every `.js` under `admin` |
| `styles.css` | `static/styles.css`, then every other `.css`, minified with lightningcss |
| `usa-map.svg` | copied from `static/` |

Scripts are minified with oxc, each parsed as a classic script so top-level
names stay global; a script that does not parse fails the build. Each bundle
is embedded with `include_bytes!` at `/assets/<name>.<hash>.<ext>` and served
with a one-year immutable cache (gzipped for browsers that accept it).
Templates link them through the constants in `templates::assets`
(`APP_JS.url`, `STYLES_CSS.url`, …).

The WASM package is built separately by `wasm-pack` into the UI directory
(`[ui_settings].ui_dir`, `crates/public_ui` in development) and served from
`/ui/pkg/` by tower-http. Pages request it with `?v=<hash>` computed at
startup, which is also cached for a year; `shared/wasm.js` loads it.
