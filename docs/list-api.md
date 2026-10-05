# List API

`GET /api/v1/competitions` and authenticated `GET /api/v1/entries` return JSON arrays. Each record retains the fields available before pagination.

The default page contains at most 50 records. It includes active competitions and competitions changed in the last 14 days. Set `history=true` to include older records. Results use descending UUID order.

| Parameter | Meaning |
| --- | --- |
| `limit` | Page size, from 1 through 100. The default is 50. |
| `cursor` | Value from the previous response's `X-Next-Cursor` header. Keep the other query parameters unchanged. |
| `ids` | Comma-separated competition IDs, or entry IDs on the entries endpoint. At most 100 IDs. Explicit IDs include history. |
| `status` | Competition group: `open`, `live`, `awaiting`, `active`, `finished`, `failed`, `cancelled`, or `all`. |
| `since` or `updated_after` | Inclusive RFC3339 change timestamp. `updated_after` takes precedence. |
| `history` | Include terminal records older than 14 days. |
| `event_id` | Entries only: restrict the authenticated player's entries to one competition, including its history. |

Continue until `X-Next-Cursor` is absent. A cursor identifies the last returned UUID; it does not create a database snapshot. Records changed between requests can appear in a later incremental poll. Keep a small overlap between change timestamps and deduplicate records by ID.

Store the `ETag` and send it in `If-None-Match` on the same URL. An unchanged page returns status 304 with no body. Entry responses use private cache validation. Request gzip to reduce response size.

Change timestamps are stored in a separate narrow table. Database triggers update it for competition, entry, ticket, and payout changes, including writes by an older application during a rolling deployment. The migration does not change contract or signing data.

Health probes check database connectivity and supervised task status. A supervised task runs database integrity scans every 15 minutes; an integrity failure stops the service.
