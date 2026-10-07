# Request ids, client addresses and browser telemetry

Every request to the public and admin listeners gets a request id, a client address, and one `http` log line.
Lines logged while handling the request end with the same id, and calls the coordinator makes to its own services carry it.
The browser can add page timings and clicks to the same log; that part is off by default.
The code is in `crates/coordinator/src/api/request_context.rs` and `api/telemetry.rs`.

## Trusted proxies

```toml
[http_context]
trusted_proxies = ["127.0.0.1", "10.0.0.0/8"]  # addresses or CIDR ranges; empty trusts nobody
client_ip_header = "X-Real-IP"
```

`COORDINATOR_TRUSTED_PROXIES` (comma-separated) and `COORDINATOR_CLIENT_IP_HEADER` override the file.
For Helm, set `config.httpContext.trustedProxies` and `config.httpContext.clientIpHeader`.

A request whose TCP peer is a trusted proxy takes its client address from `client_ip_header` and its id from `X-Request-Id`.
The proxy must overwrite both headers on every request, whatever the client sent.
Every other request is logged with its peer address and a fresh UUIDv7.
`X-Forwarded-For`, `Forwarded` and `CF-Connecting-IP` are never read, and the configuration refuses them as `client_ip_header`.
An `X-Request-Id` is used only when it matches `^[0-9A-Za-z-]{8,64}$`.

The id the request ran under is returned in the `X-Request-Id` response header.

The rate limits still use the TCP peer address (see [Request controls](REQUEST_HARDENING.md)); these settings change only what is logged.

## The request line

One line per request, at INFO with target `http`, written after the response:

```
[2026-10-07T12:00:00.000000000Z INFO] http: http rid=0192f1e0-… prid=- sid=AbCdEfGhIjKlMnOpQrStUv ip=203.0.113.7 method=GET route=/api/v1/competitions/{competition_id} status=200 ms=37 user=-
```

- `rid` is the request id; `prid` the caller's id from `X-Parent-Request-Id`, or `-`.
- `sid` is the browser tab's session id from `X-Session-Id` (`^[A-Za-z0-9_-]{16,32}$`), or `-`.
- `route` is the matched route template, or the path without its query for anything else.
- `user` is the first 16 hex characters of the public key a NIP-98 request authenticated with, or `-`.

Health checks, `/metrics` and static files (`/assets/*`, `/ui/pkg/*`, `/static/*`) get no line.
Query strings, bodies, `Authorization` headers and usernames are never logged.

Every other line logged while handling a request ends with ` rid=<id>`; the line prefix is unchanged.
Lines with targets `http`, `ui_event` and `feedback` carry their own `rid=` field and get no suffix.
A value holding a space, `"` or `=` is double-quoted, with `"` and `\` escaped; control characters are dropped and values are cut to 200 characters.

## Calls to our own services

While handling a request, the coordinator sends its id as `X-Parent-Request-Id` to the oracle, ark-swapd and the operator Grafana.
It never sends it to LND, Lightning Address hosts or other third parties, and never sends `X-Request-Id`.
Background work started on its own (watchers, refresh caches) has no request id.

ark-swapd writes the same `http` line and ` rid=` suffix with a fresh id for every request, logging the coordinator's id as `prid=`.
Its `ip=` is the TCP peer.

## Browser telemetry

```toml
[telemetry]
enabled = true
```

`COORDINATOR_TELEMETRY_ENABLED` (`true` or `false`) overrides the file; for Helm, set `config.telemetry.enabled`.
The e2e configuration turns it on.

When enabled, pages carry `<meta name="telemetry" content="on">` and `shared/telemetry.js`, part of the page bundle, sends events to `POST /api/v1/telemetry` with `navigator.sendBeacon`.
Every page also names its request in `<meta name="request-id">`, which joins the browser's events to the `http` line.
The session id is random per tab and kept in `sessionStorage` as `fdc.sid`; there are no cookies.

The beacon records:

| `ev` | Fields |
|---|---|
| `page_view` | `ref` (a same-site referrer's path, or another site's host), `ttfb`, `dcl`, `load` |
| `vitals` | `lcp`, `cls`, `inp` (the slowest interaction), sent when the page is hidden for good |
| `click` | `el`, `id`, `track` (`data-track`), and `text` for buttons and links, at most 40 characters |
| `submit` | `form` (the form's id) |
| `htmx` | `verb`, `path` (no query), `status`, `ms`, and the response's `X-Request-Id` |
| `js_error` | `msg`, `src` (file name only), `line` |
| `mark` | `name`: `login_start`, `login_ok`, `login_fail`, `signup_ok`, `pay_shown`, `pay_ok`, `entry_done` |

It never reads input, textarea or select values or editable text, and ignores everything inside `[data-telemetry="off"]`: the log-in, sign-up and password reset dialogs, the invoice field, the Lightning Address fields and the payouts address panel.
The recovery page does not load the bundle.
htmx requests also send `X-Session-Id`.
Events are sent every 10 seconds, when the page is hidden, and when it is left, in batches of at most 50; nothing is retried.

The endpoint takes `application/json` or `text/plain` bodies of at most 16 KiB (413 above that):

```json
{"sid": "AbCdEfGhIjKlMnOpQrStUv", "rid": "0192f1e0-…", "events": [{"ev": "click", "t": 1234, "page": "/", "el": "button", "text": "Log in"}]}
```

It answers 204, or 400 for a malformed body or session id.
Unknown fields are dropped, unknown event types are dropped and counted, and every string is scrubbed: anything containing `nsec1`, `npub1`, `lnbc`, `lntb`, `lnurl`, `xprv` or `tprv` (any case), a run of 40 or more hex digits, or something shaped like an email address becomes `[redacted]`.
Each tab may send 500 events an hour and the whole service 50 a second; events over either cap still get 204 and are counted in `coordinator_telemetry_events_dropped_total`.
While telemetry is off the endpoint answers 204 and logs nothing.

Each event becomes one line at INFO with target `ui_event`:

```
[… INFO] ui_event: ui_event site=5day4cast rid=0192f1e0-… sid=AbCdEfGhIjKlMnOpQrStUv ip=203.0.113.7 ev=click page=/ t=1234 el=button text="Log in"
```

`rid` is the page's request id from the beacon (or `-`), and `ip` the client address of the beacon's own request.
An `htmx` event's response id is logged as `hx_rid=`, so `rid=` always names the page.

## Synthetic traffic

synth sends `User-Agent: 5day4cast-synth/<version>` on its requests to the coordinator.
