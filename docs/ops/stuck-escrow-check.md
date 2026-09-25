# Finding Arkade escrows that hold a player's money

Read-only checks for every Arkade escrow that holds a buy-in and will not be refunded, or counted,
on its own. Nothing here writes to the coordinator, ark-swapd, or Arkade.

## Take snapshots first

Both services use SQLite in WAL mode. Query consistent copies rather than the live files:

```sh
# The coordinator: <db_settings.data_folder>/competitions.db
sqlite3 -readonly /path/to/data/competitions.db ".backup '/tmp/competitions.snapshot.db'"
# ark-swapd: <data_dir>/swaps.sqlite
sqlite3 -readonly /path/to/ark-swapd/swaps.sqlite ".backup '/tmp/swaps.snapshot.db'"
```

Run the queries below with `sqlite3 -readonly -header -column <snapshot>`. Times in ark-swapd and
in `ticket_ark_escrows`/`ticket_ark_refunds` are UNIX seconds; the other coordinator tables store
text timestamps.

## 1. ark-swapd: swaps that paid an escrow but are not finished

```sql
SELECT id, state, escrow_address, amount_sat, escrow_vtxo, ark_txid, error,
       datetime(created_at, 'unixepoch') AS created,
       datetime(updated_at, 'unixepoch') AS updated
FROM swaps
WHERE state IN ('paying_escrow', 'escrow_paid', 'unsettled')
   OR (state = 'settled' AND escrow_vtxo IS NULL)
ORDER BY created_at;
```

| State | What it means |
| --- | --- |
| `paying_escrow` | The payer's HTLC is held and the escrow payment is in flight, or its outcome was lost. Should clear within minutes. |
| `escrow_paid` | The escrow is paid and the invoice not settled yet. Should clear within seconds. |
| `settled`, no `escrow_vtxo` | The player paid, but ark-swapd never recorded the escrow VTXO, so the coordinator may not count the ticket (swap `01a0cc69…` was one). |
| `unsettled` | ark-swapd paid the escrow with its own coins, then could not settle: the player got their payment back. The escrow holds ark-swapd's money. |

Once this branch's migrations have run, the same rows also show how ark-swapd is handling them:

```sql
SELECT id, state, vtxo_lookups,
       datetime(vtxo_lookup_after, 'unixepoch') AS next_lookup,
       datetime(vtxo_lookup_gave_up_at, 'unixepoch') AS gave_up,
       pay_attempts, datetime(pay_attempted_at, 'unixepoch') AS last_payment
FROM swaps
WHERE state IN ('paying_escrow', 'escrow_paid', 'unsettled')
   OR (state = 'settled' AND escrow_vtxo IS NULL)
ORDER BY created_at;
```

A swap with `gave_up` set was looked up for about two hours and never found; check its `ark_txid`
on Arkade (section 5).

## 2. ark-swapd: refund swaps that were not claimed

```sql
SELECT id, state, amount_sat, swap_address, swap_vtxo, error,
       preimage IS NOT NULL AS player_was_paid,
       datetime(deadline, 'unixepoch') AS deadline
FROM refunds
WHERE state IN ('minted', 'paid', 'reclaimable')
ORDER BY created_at;
```

`reclaimable` with `player_was_paid = 1` means the coordinator paid the player but ark-swapd never
claimed the swap before its deadline. `minted` rows past their deadline are harmless: nothing was
signed or paid for them.

## 3. Coordinator: funded escrows of dead competitions without a settled refund

```sql
SELECT c.id AS competition,
       CASE WHEN c.cancelled_at IS NOT NULL THEN 'cancelled' ELSE 'failed' END AS ended,
       (SELECT COUNT(*) FROM tickets WHERE event_id = c.id) AS tickets,
       (SELECT COUNT(*) FROM entries WHERE event_id = c.id) AS entries,
       t.id AS ticket, e.swap_id, e.vtxo_outpoint, e.vtxo_sats,
       datetime(e.funded_at, 'unixepoch') AS funded,
       en.id AS entry,
       json_extract(p.policy_json, '$.automatic_lightning_address') AS refund_to,
       r.state AS refund_state, r.error AS refund_error,
       a.commitment_tx IS NOT NULL AS pool_funded
FROM ticket_ark_escrows e
JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
JOIN competitions c ON c.id = t.event_id
LEFT JOIN entries en ON en.ticket_id = t.id
LEFT JOIN entry_payout_policies p ON p.entry_id = en.id
LEFT JOIN ticket_ark_refunds r ON r.ticket_id = e.ticket_id
LEFT JOIN ark_funded_competitions a ON a.event_id = c.id
WHERE (c.cancelled_at IS NOT NULL OR c.failed_at IS NOT NULL)
  AND e.funded_at IS NOT NULL
  AND (r.state IS NULL OR r.state != 'settled')
ORDER BY c.id, t.id;
```

Read each row as one of these cases:

| Row | What happens |
| --- | --- |
| `pool_funded = 1` | The kickoff batch spent the escrow into the pool. Not a refund; the pool pays out or expires. |
| `entry` empty | The player paid but never entered, so they never sealed an entry key to Keymeld and nothing can sign the escrow's refund leaf. Needs an operator. |
| `refund_to` empty | The player gave no Lightning Address, and a refund can only pay one. Needs an operator. |
| `entries < tickets` | The competition never filled. Keymeld is given only the paid entries, and signs each refund with that player's own key. Refunds wait while any ticket's invoice can still be paid, because Keymeld's roster cannot change once it signs a refund. A ticket counted after that is logged as needing an operator. |
| otherwise | Refunded by cleanup once the escrow's refund leaf opens (`refund_after_start_secs` after the window starts, 24 h by default). `refund_state`/`refund_error` show progress. |

Before this branch, cleanup never picked any of these rows up.

## 4. Coordinator: Arkade tickets whose swap it has not counted

```sql
SELECT t.event_id AS competition, t.id AS ticket, e.swap_id, e.escrow_address,
       t.reserved_at, t.paid_at, t.settled_at
FROM ticket_ark_escrows e
JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
WHERE e.swap_id IS NOT NULL AND e.funded_at IS NULL
ORDER BY t.reserved_at;
```

Look each `swap_id` up in ark-swapd. With both snapshots on one machine:

```sql
-- in the coordinator snapshot
ATTACH DATABASE '/tmp/swaps.snapshot.db' AS swapd;
SELECT t.event_id AS competition, t.id AS ticket, s.id AS swap, s.state,
       s.amount_sat, s.escrow_vtxo, s.ark_txid,
       e.funded_at IS NOT NULL AS counted
FROM ticket_ark_escrows e
JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
JOIN swapd.swaps s ON s.id = e.swap_id
WHERE s.state IN ('paying_escrow', 'escrow_paid', 'settled', 'unsettled')
  AND (e.funded_at IS NULL OR s.escrow_vtxo IS NULL)
ORDER BY s.created_at;
```

Without the file, ask ark-swapd for one swap (it never returns the preimage):

```sh
curl -s -H "Authorization: Bearer $(cat <api_token_file>)" \
  "$ARK_SWAPD_URL/v1/swaps/<swap_id>" | jq
```

A `settled` swap that is not `counted` is a player who paid and was never given their entry. The
coordinator counts it once Arkade lists an unspent VTXO at the ticket's escrow address with the
ticket's price, from the swap's `escrow_vtxo` or `ark_txid`. If its competition is already over,
the ticket then appears in section 3 with no entry.

## 5. Arkade: what a VTXO holds now

Ask arkd's indexer about an outpoint, or about every VTXO of the swap's Ark transaction:

```sh
curl -s "$ARKD_URL/v1/indexer/vtxos?outpoints=<txid>:<vout>" | jq '.vtxos'
```

A spent escrow whose `spentBy`/`arkTxid` is not its refund's `ark_txid` (section 3) went somewhere
else, and the coordinator will not pay a refund for it.

## Log lines

The coordinator and ark-swapd log each of these once per ticket, swap, or competition, then at
debug while the condition lasts:

- `Cannot refund the escrow of ticket … yet: …`: a refund is blocked, with the reason.
- `Cannot sign the refunds of competition …: N of its tickets can still be paid…`: refunds wait for those invoices to expire.
- `… its ticket was counted after Keymeld was given the competition's roster…`: a ticket paid too late to join the roster; needs an operator.
- `Escrow swap … for ticket … reports its player paid; waiting for Arkade to list the escrow VTXO…`: section 4.
- `Escrow swap … paid the escrow of ticket …, but could not settle…`: an `unsettled` swap.
- `swap … paid escrow … but its VTXO was not found in N lookups…`: ark-swapd gave up looking.
