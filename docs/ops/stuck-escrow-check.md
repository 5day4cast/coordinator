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
       en.id AS entry, k.ticket_id IS NOT NULL AS registered,
       json_extract(COALESCE(p.policy_json, tp.policy_json),
                    '$.automatic_lightning_address') AS refund_to,
       r.state AS refund_state, r.error AS refund_error,
       a.commitment_tx IS NOT NULL AS pool_funded
FROM ticket_ark_escrows e
JOIN tickets t ON t.id = e.ticket_id AND t.hash = e.ticket_hash
JOIN competitions c ON c.id = t.event_id
LEFT JOIN entries en ON en.ticket_id = t.id
LEFT JOIN entry_payout_policies p ON p.entry_id = en.id
LEFT JOIN ticket_payout_policies tp ON tp.ticket_id = t.id AND tp.ticket_hash = t.hash
LEFT JOIN ticket_keymeld_registrations k ON k.ticket_id = t.id AND k.ticket_hash = t.hash
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
| `entry` empty, `registered = 1` | The player paid but never entered. Their browser sent Keymeld's registration before showing the invoice, so the refund is signed with it like an entry's. |
| `entry` empty, `registered = 0` | The player paid but never entered, and no registration was kept for the ticket, so nothing can sign the escrow's refund leaf. Needs an operator. |
| `refund_to` empty | The player gave no Lightning Address, and a refund can only pay one. Needs an operator. |
| `entries < tickets` | The competition never filled. Keymeld is given only the paid entries, and signs each refund with that player's own key. Refunds wait while any ticket's invoice can still be paid, because Keymeld's roster cannot change once it signs a refund. A ticket counted after that is logged as needing an operator. |
| otherwise | Refunded by cleanup once the escrow's refund leaf opens (`refund_after_start_secs` after the window starts: 45 minutes by default, and a day for escrows issued while the default was a day). `refund_state`/`refund_error` show progress. |

Before this branch, cleanup never picked any of these rows up.

`refund_error` = `the escrow is held by a queued Arkade batch intent` means Arkade refused the
refund with `VTXO_ALREADY_REGISTERED`: a kickoff that never finished left its batch intent
queued, and arkd keeps it until a batch confirms it or its owner deletes it. Cleanup deletes the
intent with a proof over the escrow that pays nothing, then submits the refund again. The note
stays while the delete fails, for instance because Keymeld's verifier predates delete proofs;
the refund then keeps its swap instead of minting a new one each hour, and the log says why. An
arkd operator can also remove the intent: `GET /v1/admin/intents` lists it, and
`POST /v1/admin/intents/delete` deletes it. The next cleanup pass then refunds the escrow.

`refund_error` starting `the escrow's VTXO expired on Arkade` means Arkade refused the refund
with `VTXO_RECOVERABLE`, or lists the escrow as expired or swept (section 5: `expiresAt`,
`isSwept`). A VTXO expires with the coin that paid it, and arkd then spends it only in a batch.

- While Arkade lists the escrow as expired and not swept, the refund is held: it keeps its swap,
  and nothing is minted or signed for it. An escrow found expired before any refund was minted
  has no refund row yet; the log says why.
- Once Arkade lists it as swept (`isSwept: true`), cleanup recovers it in a batch: an intent
  spends the escrow's refund leaf, signed by Keymeld as the player, and the batch pays the
  refund's swap as a new VTXO. The refund then reads `submitted`, with the batch's commitment
  transaction as its `ark_txid`, and is paid and settled like any other. A stale swap is
  replaced first, so the refund may get a new `refund_id`.
- If the note goes on with `: … recovering it in a batch failed, and is tried again: …`, the
  last attempt failed for the reason given. It is tried again after ten minutes, and while it
  keeps failing a stale swap is replaced at most every six hours. Common reasons:
  - Keymeld's verifier predates refunds in a batch (`Unsupported Coordinator verifier action`,
    or a refusal to decode the spend): deploy the verifier enclave built from this version.
  - `INTENT_INSUFFICIENT_FEE`: the Arkade server charges for the recovery, and the swap must
    receive the escrow's whole value. Needs an operator.
  - `timed out waiting for a batch to select the recovery intent`: the server ran no batch in
    time. The attempt deleted its intent, and the next one registers again.

A refund left `minted` whose escrow Arkade lists as settled (`settledBy`) was recovered by a
batch this coordinator stopped following, for example across a restart. Cleanup finds the
swap's VTXO from that batch and goes on; if the swap holds none, the log says the escrow was
spent by something other than its refund.

### Writing off a refund that can never finish

Some rows above never finish on their own: `entry` empty with `registered = 0` (nothing can sign
the refund leaf), a ticket counted after Keymeld had the roster, a player who gave no Lightning
Address, or an escrow spent by something other than its refund. Cleanup retries them on every
pass, the log says `… it needs an operator` for each, and the competition's pages keep saying
`Refunding… 0 of N paid entry fees returned so far`.

Once the operator has decided not to refund such an escrow (or has settled with the player some
other way), write it off:

```sh
coordinator admin write-off-refund --ticket <ticket-id> --reason "<why>" --yes
# every stuck refund of one competition; the others are listed and left alone
coordinator admin write-off-refund --competition <competition-id> --reason "<why>" --yes
```

This writes to the coordinator's database and nothing else: the escrow stays where it is on
Arkade. From then on cleanup skips the escrow without logging it, the competition leaves the
cleanup queue once nothing else is owed, and the pages count the escrow as no longer owed: it
drops out of the `N of M` count and of `Refunds open …`, and a competition whose other escrows are
refunded reads `Refunded`, with a note of how many fees were not returned.
`coordinator admin competitions show <id>` lists each write-off with its reason, and the JSON
(`GET /api/v1/admin/competitions/{id}`) carries them in `refund_write_offs`. The reason and time
are kept in `ticket_ark_refund_write_offs`.

A ticket's refund is written off without `--force` only when it is stuck: no refund was ever
minted and its player never sent a registration, or this coordinator process last logged it as
needing an operator (a restart forgets that until the next cleanup pass logs it again). Any other
refund still in progress is refused unless `--force`; a forced write-off stops cleanup where the
refund stopped, so check sections 2 and 5 first. A settled refund, one already written off, an
escrow a batch spent into its pool, and one whose competition has not been cancelled or failed
are always refused. The endpoint is `POST /api/v1/admin/refunds/write-off` with
`{"ticket_id" | "competition_id", "reason", "force"}`.

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

The coordinator also refuses to count a ticket whose escrow VTXO expires too soon: before the
escrow's refund locktime plus `escrow_expiry_margin_secs` (six hours by default). A preconfirmed
VTXO expires with the coin that paid it, so this means ark-swapd paid from a coin near the end of
its life. The log says `Escrow swap … for ticket … paid … with a coin that expires on Arkade at
…`. The player paid and has no ticket, so settle with them directly; the escrow holds
ark-swapd's coins, which expire back to the Arkade server. Check the expiry with section 5
(`expiresAt`), and renew ark-swapd's coins before more tickets are sold.

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
- `Wrote off the refund of ticket … in competition …`: an operator wrote off its refund (see above).
- `Cannot refund … yet: its escrow … is held by an Arkade batch intent that cannot be deleted yet…`: section 3, `refund_error`.
- `Deleted the Arkade batch intent that held the escrow of ticket …`: a refund freed its escrow, and every other escrow of that intent.
- `Cannot refund … yet: its escrow … holds … sats, but its VTXO expired on Arkade…`: section 3, `refund_error`; held until Arkade has swept it.
- `Cannot refund … yet: its escrow … holds … sats and its VTXO expired on Arkade; recovering it in a batch failed, and is tried again: …`: section 3, `refund_error`.
- `Recovered the expired escrow of ticket … into its swap, as … in batch … (commitment …)`: a batch gave an expired escrow's value back; its refund is paid next.
- `The expired escrow of ticket … was recovered into its swap by the batch of commitment …`: the same, found after the batch finished unseen.
- `kickoff intent … may still hold the escrows, since deleting it failed…`: a kickoff failed and left its intent queued; the pool's refunds, or its next kickoff, delete it.
- `Cannot sign the refunds of competition …: N of its tickets can still be paid…`: refunds wait for those invoices to expire.
- `… its ticket was counted after Keymeld was given the competition's roster…`: a ticket paid too late to join the roster; needs an operator.
- `Escrow swap … for ticket … reports its player paid; waiting for Arkade to list the escrow VTXO…`: section 4.
- `Escrow swap … for ticket … paid … with a coin that expires on Arkade at …`: section 4; the ticket is not counted.
- `Escrow swap … paid the escrow of ticket …, but could not settle…`: an `unsettled` swap.
- `swap … paid escrow … but its VTXO was not found in N lookups…`: ark-swapd gave up looking.
