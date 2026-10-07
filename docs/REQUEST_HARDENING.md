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

## Sign-up proof of work

Accounts are free, so rate limits alone cannot stop a bot from creating them by the thousand without also locking out a crowd behind one shared address.
With `[pow]` enabled, every account creation must carry a small proof of work instead: nothing to a person, a cost per account to a bot.
It is off by default.

```toml
[pow]
enabled = true
base_bits = 18     # about a second on a phone; each bit doubles it
max_bits = 22      # at most 32
step_signups = 200 # one more bit per this many accounts created in the last hour
```

`POST /api/v1/users/pow` returns `{"challenge", "difficulty", "expires_at"}`, or 204 while proofs are off.
It sits under the user API's stricter rate limit.
The challenge is 41 bytes in base64url without padding: 16 random bytes, `expires_at` (u64 big-endian), the difficulty (u8), and the first 16 bytes of an HMAC-SHA256 over those 25 bytes.
A solution is a u64 nonce such that SHA-256(challenge bytes ‖ nonce big-endian) starts with at least `difficulty` zero bits.
`POST /api/v1/users/register` and `POST /api/v1/users/username/register` take it as `pow_challenge` and `pow_nonce` (decimal text).
The server checks the HMAC, the ten-minute expiry, that the challenge's difficulty is at least the one required now, the hash, and that the challenge was not spent before; it then spends the challenge.
A refusal is HTTP 400 with `"code": "pow_rejected"`; the sign-up dialog solves a fresh challenge and sends once more before showing it.
Logins and password changes never need a proof.

The difficulty is the same for every client address: `base_bits`, plus one bit for every `step_signups` accounts created in the last hour across the whole service, up to `max_bits`.
The count is read from the users database at most every 15 seconds.
Challenges are stateless, signed with a key made at start, so a restart voids outstanding challenges; browsers fetch another.
Spent challenges are process-local and kept until they expire.

The sign-up dialog fetches a challenge when it opens and solves it in a Web Worker (`static/pow-worker.js`, with a small SHA-256 in `static/sha256.js`, both served from this site).
A sign-up sent before the solution is ready waits, its button reading "Preparing…".
With the metrics listener on, `coordinator_signup_pow_checks_total{result}` counts proofs verified and refused by reason, and `coordinator_signup_pow_difficulty_bits` reports the difficulty required.
The e2e configuration leaves proofs off.
