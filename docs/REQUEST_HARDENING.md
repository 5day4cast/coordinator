# Public request controls

Configure exact HTTP or HTTPS origins in `api_settings.origins`, `ui_settings.remote_url`, and `ui_settings.private_url`.
NIP-98 authentication accepts only these origins, with the request's exact path, query, and method.
Wildcards, credentials, paths, queries, and fragments are invalid in origin configuration.
The application does not derive the signed origin from `Host` or forwarding headers.

Rate limits use the TCP peer address. Forwarding headers cannot change the rate-limit bucket.
Clients behind one reverse proxy share that proxy's bucket.
Set limits for that aggregate traffic, and configure per-client limits at the trusted proxy when needed.

```toml
[api_settings]
origins = ["https://weather.example"]
replay_capacity = 100000

[api_settings.rate_limit]
enabled = true
per_second = 20
burst = 60
auth_per_second = 2
auth_burst = 10
```

The general limit covers public HTML and API routes. The stricter limit also covers the user API, including login and password reset.
Static `/ui/*` assets are excluded. A depleted bucket returns HTTP 429.
The admin listener retains its separate token and session authentication.

For Helm, set `config.api.origins`, `config.api.replayCapacity`, and `config.api.rateLimit`.
Replace the default localhost origin with the deployed browser origin.
The e2e configuration raises limits because tests share one peer address.

The replay guard retains valid event identifiers and removes expired identifiers every 30 seconds or during full-capacity admission.
It never evicts live identifiers to accept a new request. Full capacity returns HTTP 503.
Replay state is process-local; multiple active replicas require shared replay storage.

Unexpected worker completion, errors, panics, and task abortion trigger service shutdown.
This includes the automatic payout worker. Recoverable errors handled within worker loops retain their existing retry behavior.
The coordinator loads its signing key once at startup and erases its cached bytes on drop.
Restart the service after an intentional key-file replacement; do not rotate keys while competitions remain active.
