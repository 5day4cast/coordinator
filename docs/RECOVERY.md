# Fund recovery records

A player's money sits in an Arkade escrow before kickoff and in a DLC after it. Both can be spent without the coordinator, but only with data that until now lived in the coordinator's database: the wallet seed blob, the entry id the entry key is derived from, the escrow's scripts, the ticket preimage, and the signed contract.

The coordinator therefore publishes that data to Nostr relays as it changes, and offers each player a recovery file with the same contents. With their nsec and either the relays or the file, a player can recover every entry with a recovery tool, without the coordinator, its database or Keymeld.

The shared format is fixed in the recovery spec (kept with the recovery tool); this page describes what the coordinator publishes and why it is safe to publish.

## Configuration

Off by default. In the coordinator's settings file:

```toml
[recovery]
enabled = true
# wss:// relays the records go to. Empty keeps the records for the recovery file only.
relays = ["wss://relay.example.org", "wss://nos.lol", "wss://relay.damus.io"]
# Signs and encrypts the records, and nothing else. Created on first start.
key_file = "./creds/coordinator_recovery_key.pem"
```

- The recovery key is not the coordinator's market-maker key, and the coordinator refuses to start if `key_file` names `coordinator_settings.private_key_file`.
- Players find their records by this key: back the file up with the other credentials, and give every instance of one deployment (both blue/green slots) the same file. A new key starts a new set of records; the old ones stay readable with the old key's public key.
- `GET /api/v1/recovery/info` returns the key's public key, the network and the relays, and 404 while recovery is off.

## What is published

Every event is kind 30078 (NIP-78), replaceable by its `d` tag, and signed by the recovery key.

| Event | `d` tag | Other tags | Content |
| --- | --- | --- | --- |
| Wallet backup | `<blind>:wallet` | `["b", <blind>]` | NIP-44 to the player: the `users.encrypted_bitcoin_private_key` blob, unchanged |
| Entry record | `<blind>:entry:<entry_id>` | `["b", <blind>]` | NIP-44 to the player: the entry's record (below) |
| Competition contract | `competition:<id>` (and `competition:<id>:part:<n>`) | `["c", <id>]` | Plain JSON: contract parameters, signed contract, funding outpoint, oracle, attestation |

`blind = hex(sha256("coordinator-recovery/v1" || coordinator_pubkey || user_pubkey))`, both keys as 32-byte x-only keys. Only someone who knows the player's pubkey and the coordinator's recovery pubkey can compute it, and no event carries a `p` tag.

### Entry records

An entry record holds the competition and entry ids, the entry's public key, its status (`ticketed`, `escrowed`, `paid`, `in_contract`, `won`, `lost`, `refunded`, `settled`), and:

- `escrow`: the Arkade escrow's leaf scripts in leaf order, its refund locktime `T`, exit and unilateral refund delays, the Arkade server's key and URL, and the escrow VTXO once funded.
- `ticket`: the ticket hash, and its preimage once the payment settled (the player learned it by paying).
- `contract`: the funding outpoint, the player's index, the sha256 of the contract parameters as dlctix serializes them, the player's pruned contract signatures (dlctix `pruned_signatures`), and the relative locktime.

The entry key and the payout preimage are never in a record. The recovery tool derives both from the wallet seed, the network and the entry id.

A record is rebuilt whenever its competition, entries, tickets or payouts change, and published again only when its contents changed. A ticket whose entry is not submitted yet gets a record as soon as its payout policy names the entry id, so an escrow paid before the entry is submitted is covered.

### Competition contract

Published once the contract is signed, and again when the attestation is known. It is the same data `GET /api/v1/competitions/{id}` serves. A content over 60 KB is gzipped and base64 encoded (`"encoding": "gzip+base64"`); if that is still over 60 KB, `competition:<id>` holds a manifest with `"parts": N` and no data, and `competition:<id>:part:1` to `:part:N` hold the data in order. A pool of 20 players paying 2 places has 381 outcomes and 800 win conditions; its contract event is about 250 KB as JSON and 130 KB gzipped, so it goes out as a manifest and 3 parts. Each player's own record carries only their 39 outcome and 40 split signatures: about 13 KB, under 20 KB encrypted.

## The recovery file

The Payouts page has a "Download recovery file" link. The file is served by `GET /api/v1/recovery/kit`, which needs the player's NIP-98 signature and only ever reads that player's records:

```json
{ "type": "coordinator-recovery-kit", "v": 1, "network": "signet", "created_at": 0,
  "coordinator_pubkey": "hex", "user_pubkey": "hex", "relays": ["wss://..."],
  "wallet": "<NIP-44 ciphertext>", "entries": ["<NIP-44 ciphertext>", "..."],
  "competitions": [ { "...": "contents of the competition events" } ] }
```

The wallet and entries are the same ciphertexts as the events, so the file is safe to store anywhere: without the nsec it reveals only which competitions the player entered. It is built from the records the coordinator last published, so it is current to within a few seconds.

Where the nsec is shown once at sign-up, one line tells the player that the nsec and this file (or the relays) are all they need.

## Publishing

A background task, under a worker lease so one instance publishes at a time, reads the competitions changed since its last look every 5 seconds, plus a few competitions and wallets per tick from a full pass. The full pass starts at launch, which backfills records for competitions created before this ran, and repeats every 30 minutes.

New versions wait in the `recovery_outbox` table. Each event is offered to every relay that has not taken it yet; a refusal or an unreachable relay is retried after a minute, doubling up to an hour. After 8 attempts an event that at least one relay took is not offered again; one no relay took keeps being retried hourly. Nothing in the entry, kickoff, payout or refund paths waits for this task or for a relay.

Metrics:

- `coordinator_recovery_relay_publishes_total{result="accepted"|"failed"}`: events offered to a relay, by result.
- `coordinator_recovery_outbox_depth`: events not yet taken by every relay.

The outbox table is additive: an older coordinator running beside a newer one ignores it.

## Threat model

- Relays see the recovery key's events, their sizes and times, blind tags and ciphertext. They cannot tell which player an entry or wallet event belongs to, or link two players' events, without the player's pubkey. Events of one player share a blind tag, so a relay can count one player's entries and see when they change.
- Entry records are NIP-44 v2 between the recovery key and the player. Anyone holding the recovery key can read them, so it is as sensitive as the database, which holds the same data.
- The ticket preimage is published only after the player paid, when they hold it already.
- The wallet blob is the one the browser stored at sign-up, encrypted to the player's nsec. Publishing it adds no exposure beyond a relay seeing a second ciphertext of it.
- Competition events are public data the API already serves; the contract names entry keys but no Nostr identities.
- A relay can drop or withhold events. Several relays and the downloadable file cover that. A relay cannot forge an event: every event is signed by the recovery key, whose public key the tool takes from the file or `/api/v1/recovery/info`.
