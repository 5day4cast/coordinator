# Templates & Frontend Assets

Pages are rendered on the server with [Maud](https://maud.lambda.xyz/) and
updated with [htmx](https://four.htmx.org/) 4. JavaScript is kept for what only
the browser can do: the WASM wallet (keys, signing, payment), the htmx
extension that signs account requests, and a few small page behaviours.

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
| `htmx.js` | `vendor/htmx/4.0.0/htmx.min.js` at the workspace root, as published |
| `theme.js` | `static/theme-init.js`, loaded before first paint |
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

## Conventions

- Handlers answer htmx requests (`HX-Request`) with content only and direct
  visits, reloads and Back (`HX-History-Restore-Request`) with the whole page,
  so every URL works when reloaded or shared.
- htmx 4 attributes apply only to the element they are on (no inheritance
  without `:inherited`). Error responses are not swapped in unless the element
  asks with `hx-status:<code>`; fragments that show a server error use 500.
- Account pages (`/entries`, `/payouts`) and a ticket's status are signed with
  NIP-98 by `shared/htmx_auth.js`; opened directly they show a log-in prompt
  that loads the page once the visitor logs in (the `fw:login` event).
- Fragments that need the oracle's weather wait briefly on the leaderboard
  cache and otherwise show `fragments::loading`'s placeholder.
- Times are rendered as `<time datetime=… data-local>` in UTC and shown in the
  reader's time zone by `shared/page.js`.
- Prefer attributes (`hx-get`, `data-open-modal`, `data-copy`) and delegated
  listeners over inline `onclick` and per-element setup.
