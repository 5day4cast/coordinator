# Disaster recovery: restore the coordinator and settle what it owes

When the coordinator's host or its databases are lost, restore them from backups and run the
coordinator in **settle-only mode** until every obligation it holds is finished. In that mode it
takes no new money: no competition, ticket, entry, swap or escrow is created. It still kicks off,
settles, pays and refunds everything already paid for.

Paths, hosts and buckets below are placeholders. Run each step on the host that will run the
coordinator, with the coordinator stopped until step 4.

## What a restore needs

| What | Where it lives | Backed up by |
| --- | --- | --- |
| `competitions.db`, `users.db` | `db_settings.data_folder` | Litestream, to object storage |
| Market-maker key | `coordinator_settings.private_key_file` and `bitcoin_settings.seed_path` (by default both `./creds/coordinator_private_key.pem`) | A separate, offline copy of the creds directory |
| LND macaroon and TLS certificate | `ln_settings.macaroon_file_path`, `ln_settings.tls_cert_path` | The creds backup, or new ones from LND |
| Operator token | `admin_settings.token_file` | The creds backup, or a new one |
| ark-swapd token | `ark_settings.swap_token_file` | The creds backup, or a new one from ark-swapd |
| `Settings.toml` | The deployment's configuration | The deployment repository |

The market-maker key is the one secret that cannot be replaced. It signs every contract the
coordinator is in, reclaims unpaid winners' outputs and the coordinator's escrows, and the
databases record its public key. Without it, money held in contracts can only move by the players'
own paths (see [what players can do](#5-what-players-can-do-meanwhile)).

LND, arkd, ark-swapd, Keymeld and the oracle keep their own state. This runbook assumes they are
up, or says what still works when one is not.

## 1. Restore the databases from Litestream

1. Stop the coordinator and its Litestream replicator, on every host and slot that runs them. Two
   processes must never write to one database during a restore.
2. Restore each database to a scratch path first, not over the data folder:

   ```sh
   litestream restore -config /path/to/litestream.yml \
     -o /restore/competitions.db /path/to/data/competitions.db
   litestream restore -config /path/to/litestream.yml \
     -o /restore/users.db /path/to/data/users.db
   ```

   Without `-timestamp` this restores the newest state the replica holds, which is what you want:
   the closer to the moment of loss, the less the coordinator has to reconcile. Restore both
   databases to the same moment if you restore to a point in time. Check how far back the replica
   reaches before relying on a point in time: snapshot retention is a top-level Litestream
   setting, and a replica may only hold the last day.
3. Check each copy:

   ```sh
   sqlite3 /restore/competitions.db 'PRAGMA integrity_check;'   # prints "ok"
   sqlite3 /restore/users.db 'PRAGMA integrity_check;'
   sqlite3 /restore/competitions.db \
     "SELECT max(updated_at) FROM list_updates;"   # the newest write the backup holds
   ```

   Note the newest write. Anything the coordinator did after it is missing from the database:
   step 4 reconciles what it can.
4. Move the copies into the data folder, keeping whatever was there aside rather than deleting it:

   ```sh
   mkdir -p /path/to/data.before-restore && mv /path/to/data/*.db* /path/to/data.before-restore/
   cp /restore/competitions.db /restore/users.db /path/to/data/
   ```

5. Start Litestream again only after the coordinator has started and you have checked step 4,
   and point it at a fresh replica path if you want to keep the old replica's history untouched.

## 2. Restore the market-maker key and the other creds

1. Copy the creds backup into place with owner-only permissions:

   ```sh
   install -m 600 /backup/creds/coordinator_private_key.pem /path/to/creds/coordinator_private_key.pem
   ```

   Do this **before the first start**. The coordinator creates a new key when the file is
   missing. A restored database then refuses it, since its stored public key differs, and the
   coordinator does not start. If that happens, delete the generated file and restore the backup.
2. Put back, or issue again, the LND macaroon and TLS certificate, the operator token and the
   ark-swapd token. These can be replaced; the market-maker key cannot.
3. Check the key matches the database before going further:

   ```sh
   sqlite3 /path/to/data/competitions.db 'SELECT name, hex(pubkey) FROM coordinator_metadata;'
   ```

   That is the x-only public key of the market-maker key. The coordinator compares the two at
   start and refuses to start on a mismatch.

## 3. Start in settle-only mode

Turn the mode on in `Settings.toml`:

```toml
[coordinator_settings]
settle_only = true
# What to do with competitions and pools that have not kicked off:
# "refund" (default) cancels them and refunds every entry; "kickoff" starts those whose entries
# are already paid.
settle_only_unstarted = "refund"
```

or set `COORDINATOR_SETTLE_ONLY=true` in the service's environment, which overrides the file.
When two coordinator slots share the database, give both the same setting.

While it is on:

- New competitions (from the admin page, the CLI, the API or synthetic traffic), tickets, entries,
  swaps and escrows are refused with `503 Entries are paused`. `GET /api/v1/network-fee` answers
  503 too, so synthetic traffic holds off by itself.
- The competitions page and the entry form show a plain "Entries are paused." banner and nothing
  else. Players see no operator detail.
- Everything that settles money keeps running: kickoffs (with `"kickoff"`), attestation polling,
  outcome, expiry and split broadcasts, Lightning payouts and the payout window cutoff, hold invoice
  cancellations, Arkade escrow refunds and recoveries, and market-maker reclaims.
- With `"refund"`, a competition or pool that has no contract yet and that no Arkade batch has
  funded is cancelled at its next step, and the cleanup sweep refunds its entries. One whose
  contract is built carries on: its kickoff is already under way.
- `GET /api/v1/health_check` returns `{"status":"ok","settle_only":true}`, and the metric
  `coordinator_settle_only` is 1.

Turning the mode on with `"refund"` cancels every competition that has not kicked off. That
cannot be undone, so use `"kickoff"` if the pools that are filling should still run.

## 4. Verify the reconciliation

Every start compares the restored database with what already happened. Payouts wait until the
comparison has run once.

- **Chain.** A settlement transaction the chain already holds counts as broadcast, so a
  competition whose database is behind the chain records it and moves on rather than retrying a
  refused broadcast. This covers the funding, outcome, expiry and split transactions, which are
  fixed by the contract, and any reclaim the coordinator built the same way again.
- **LND, failed payouts.** A payout the database recorded as failed but LND paid is marked paid,
  with LND's proof of payment.
- **LND, payouts the database never saw.** A payout made after the backup's newest write is not in
  the database, while LND paid it. Each payment LND sent after that write that no payout or refund
  in the database made, and whose amount is one an unpaid entry of an unsettled contract could be
  owed, **holds** that entry's Lightning payout. Nothing is paid to a held entry until an operator
  releases it. The winner's on-chain claim is not affected.

Check, in order:

1. The coordinator log has `Restore reconciliation found the database in step with LND`, or a
   `Restore reconciliation:` line with what it changed, and one `Lightning payout held` line per
   hold. `Restore reconciliation failed; payouts wait for it` means LND did not answer; it retries
   every 30 seconds.
2. `coordinator admin payout-holds list` shows the holds. For each, look the payment up in LND
   (`lncli trackpayment <hash>` or the payment list): if it paid this entry's winner, leave the
   hold, since the winner has been paid; if it paid something else, release it:

   ```sh
   coordinator admin payout-holds release <entry-id>
   ```

   Resolve holds well within the contract's reclaim window: a held winner who neither is paid
   over Lightning nor claims on chain is reclaimed by the market maker once the split outputs
   mature. The `coordinator_payout_holds` gauge counts the holds still open.
3. `coordinator admin competitions list --state active`: every unfinished competition should move
   within a few sweeps. `coordinator admin competitions show <id>` lists the errors each one kept.
   Compare the funding, outcome and split transactions with a block explorer.
4. `coordinator_payout_jobs_open`, `coordinator_payout_jobs_failed` and the refund progress on
   the admin funds page fall as the backlog settles.

What the reconciliation cannot repair, and how to handle it:

- **A contract built after the backup's newest write.** Its signed contract is not in the
  database. If it was funded, the funding output is on chain under a contract only the players and
  the public competition record still hold. Players recover through the recovery tool.
- **An Arkade kickoff whose batch ran after the newest write.** The escrows were spent into the
  contract, but the database does not know the batch. The kickoff fails at its deadline and its
  refunds cannot spend the escrows. Find the commitment transaction from arkd, then write off the
  refunds (`coordinator admin write-off-refund --competition <id>`).
- **A refund paid after the newest write.** The escrow was already recovered, so the refund is not
  made again; it stays pending. Check it against LND before writing it off.

## 5. What players can do meanwhile

Players do not need the coordinator to get their money back. With their nsec, or the recovery
file from their account page, the recovery tool ([RECOVERY.md](../RECOVERY.md)) can:

- refund an Arkade escrow that never kicked off, with the player's key alone once its refund
  locktime has passed;
- broadcast a contract's outcome transaction, or its expiry transaction once the contract has
  expired unattested, and then the split transaction;
- claim a win on chain once the split transaction has the required confirmations.

The market maker can reclaim a winner's split output a further delay after that, so a winner who
was not paid over Lightning should claim before then.

## 6. When Keymeld is unavailable

Keymeld holds the players' deposited entry keys and signs on their behalf. Without it:

Still works:

- Broadcasting the pre-signed outcome and expiry transactions of every signed contract, and the
  split transactions, which the coordinator signs with ticket preimages it already holds.
- Market-maker reclaims of unpaid winners' split outputs, and reclaims of the coordinator's own
  escrows: they need only the market-maker key.
- Cancelling hold invoices, so single competitions paid by hold invoice are refunded.
- Lightning payouts for entries without a payout escrow: the winner reveals the payout preimage and
  pastes an invoice, and the coordinator pays it.
- Attestation polling and the payout window cutoff.

Does not work until Keymeld is back:

- Kickoffs: no new contract can be signed. Pools waiting to kick off fail at their deadline and
  are refunded.
- Refunds of Arkade escrows held for queued competitions and pools: they are signed with the
  players' deposited keys. Players can refund themselves after the refund locktime with the
  recovery tool.
- Automatic payouts, and invoice payouts for entries with a payout escrow, which Keymeld releases.
  Winners keep their on-chain claim.

## 7. Leaving settle-only mode

Once every unfinished competition has settled or been refunded, every payout hold is resolved,
and the Litestream replica is healthy again, set `settle_only = false` (and unset
`COORDINATOR_SETTLE_ONLY`) and restart. `GET /api/v1/health_check` then reports
`"settle_only":false` and new competitions can be created.
