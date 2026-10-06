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

## Recovering without the coordinator

With only their nsec, or the recovery file and their nsec, a player can find every entry and move its money without the coordinator, its database or Keymeld. Two tools share one core (`crates/coordinator-recover`):

- **The recovery page**: served by the coordinator at `/recover`, and released as static files that work anywhere once the coordinator is gone (below).
- **The CLI**, `coordinator-recover`, released for Linux (x86_64, aarch64) and macOS. It also handles Arkade escrows, which the page cannot.

### What it reads

1. The player's records: kind 30078 events by the coordinator's recovery key, tagged `["b", blind]`, from the relays the recovery file names (or `--relays`, or a few public relays). `blind = sha256("coordinator-recovery/v1" || coordinator_pubkey || user_pubkey)`. The recovery file holds the same ciphertexts.
2. The wallet seed: the record is NIP-44 from the recovery key to the player; inside it, the browser wallet's backup is NIP-44 from the player to themselves. Entry keys and payout preimages are derived from the seed, the network and the entry id, exactly as the browser wallet derives them.
3. Each competition's contract (tag `["c", id]`), whole, gzipped, or as a manifest and parts.
4. The oracle's attestation: from the competition record, the oracle's Nostr record (kind 30078, `d` = `oracle:<event id>`, see noaa-oracle's `docs/NOSTR.md`), or the oracle's API (`--oracle`). Any value is accepted only if it opens one of the contract's locking points, so where it came from does not matter.
5. Chain state from any Esplora API: mempool.space on mainnet and Mutinynet's on signet by default (`--esplora`).

Nothing is signed for an entry unless the key derived from the seed is the key its record names, and the contract has that key as a player with this wallet's payout hash. Every contract signature this player relies on is verified before it is used.

### The CLI

```sh
coordinator-recover --kit coordinator-recovery-npub1….json inspect
coordinator-recover --network signet --coordinator-pubkey <hex> inspect   # nsec only
coordinator-recover claim --to bc1q… [--fee-rate 5] [--ticket-preimage <hex>] [--dry-run]
coordinator-recover claim --to bc1q… --fee-rate 40 --fee-utxo <txid>:<vout> --fee-change bc1q…   # key from COORDINATOR_RECOVER_FEE_KEY
coordinator-recover refund-escrow --entry <id> --to <ark address> [--dry-run]
coordinator-recover unroll --entry <id> [--to bc1q…] [--fee-utxo … --fee-change …] [--dry-run]
```

The nsec comes from `--nsec`, `COORDINATOR_RECOVER_NSEC`, or a prompt that does not echo it. It is never printed. Without the recovery file, the coordinator's recovery pubkey is needed (`--coordinator-pubkey`); `GET /api/v1/recovery/info` serves it while the coordinator is up.

- **`inspect`**: per entry, where the money is (escrow VTXO, funding output, outcome or split output, spent), what can be done now, and when the next step opens, by block height or time. It warns when the market maker's reclaim (`2·delta` blocks after an outcome or split transaction confirms) is near or open.
- **`claim`**: broadcasts, skipping what is already on chain, the outcome transaction (the attestation adapts its signature) or, past the event's expiry, the expiry transaction (equal shares); then the split transaction with the player's ticket preimage; then, `delta` blocks after the split confirms, the win transaction, signed with the entry key, to `--to`. Run it again as each step confirms. `--dry-run` prints the raw transactions instead.
- **`refund-escrow`**: from the refund locktime `T` on, moves the whole escrow through its refund leaf with the Arkade server's signature to the player's Ark address. If the VTXO has expired and been swept, it registers a recovery intent instead and signs the batch that pays it.
- **`unroll`**: without the Arkade server's cooperation, puts the escrow on chain and then sweeps it through the player's own leaf once its delay has passed.

#### Fee bumping

Contracts built with anchors (dlctix 0.2, `dlc_anchor_settings.enabled`, see [QUEUED_COMPETITIONS.md](QUEUED_COMPETITIONS.md#anchors)) carry a 240 sat pay-to-anchor output on every outcome, expiry and split transaction. Those transactions still pay the contract's fee rate and relay on their own. When one pays less than the claim's `--fee-rate` (by default Esplora's six-block estimate), `claim` builds a CPFP child that spends the anchor and one coin of the player's, so that the two together pay `--fee-rate`, and broadcasts them as a package through Esplora's `POST /txs/package`, or one after the other where the Esplora has none. It does this both for a transaction it is about to broadcast and for one already in the mempool and not yet confirmed.

- **The coin.** `--fee-utxo <txid>:<vout>` and its private key in WIF, from `COORDINATOR_RECOVER_FEE_KEY` (or `--fee-key`, which leaves it in the shell history). The coin must pay that key's P2WPKH address or its single-key P2TR address (BIP 86, no script tree), as single-key wallets make them; the tool reads the coin's value and script from Esplora, refuses a spent coin or a key that does not match, and never prints the key. Use a small coin set aside for this, not a key that guards anything else: the key signs only the child's own input. A PSBT for an external wallet would keep the key out of the tool, but needs a second round trip per child; a single-purpose coin is the simpler safe option.
- **The change.** `--fee-change <address>`: everything the child does not pay in fees, including the anchor's 240 sats, goes there.
- **One child per run.** The coin is spent by the first child, so `claim` bumps at most one transaction per run; run it again with another coin for the next.
- `--dry-run` prints the child after its parent instead of broadcasting either.
- Contracts built without anchors are unchanged: the tool says each of their transactions "has no anchor and cannot be fee bumped", and does not touch the fee coin.
- An anchor has no key, so anyone may attach a large low-rate child to it (pinning). Bump early, before the `delta` window closes.

`unroll` uses the same coin to pay for each Arkade virtual transaction, which pays no fee and has a zero-value anchor. Those are TRUC (version 3) transactions, so the child is version 3 too, and the pair must go out as a package.

#### What the CLI cannot do yet

- **Fee bumping contracts without anchors.** Contracts built before anchors (dlctix 0.1) have outcome, expiry and split transactions that pay the fee rate fixed at signing and no anchor output; nobody can bump them, and the tool says so for each one. The win and unroll sweep transactions are the player's own and can be signed again at a higher `--fee-rate`.
- **Fee bumping from the page.** The recovery page says which transactions have an anchor, but builds no child: it holds no coin of the player's. Use the CLI's `--fee-utxo`.
- **On-chain refunds of a live escrow.** A live VTXO is refunded offchain to an Ark address; leaving Arkade from there is any Arkade wallet's offboard. (Offboarding directly needs a forfeit through the refund leaf in a batch, which is not built.)
- **Unroll without arkd.** The escrow's virtual transactions come from arkd's indexer; the records do not carry them. With arkd down, an escrow can be unrolled only if its ancestry was saved elsewhere.
- **Unroll fees without a fee coin.** Without `--fee-utxo`, the tool prints each virtual transaction and its anchor, and the child comes from the player's own wallet (for example `bitcoin-cli submitpackage`).

### The recovery page

The page does the DLC flows of the CLI (find records, inspect, build and broadcast the outcome or expiry, split and win transactions) in the browser. Escrow refunds and unrolls need arkd's gRPC API and stay in the CLI. The page's script only fetches what the WASM module asks for and shows its answers; the nsec goes into the module and the form field is cleared.

On the coordinator, `/recover` is a server-rendered shell around `templates/static/recover.js` and the WASM package the site already serves. Its Content-Security-Policy is the public pages' own, except that it may connect to any `https:` or `wss:` origin, for the relays and Esplora the player chooses.

#### Hosting it anywhere

Each release has `coordinator-recover-page-<version>.tar.gz`: `index.html`, `recover.js`, `recover.css`, Bulma and the WASM package in `pkg/`, with checksums. To build it from source:

```sh
scripts/build-recover-page.sh out/recover            # builds the WASM package too
scripts/build-recover-page.sh out/recover wasm-pkg   # or reuses one
```

Serve the directory from any static host: GitHub Pages, IPFS, an S3 bucket, or `python3 -m http.server` on the player's own machine. Browsers will not load the module from a `file://` page. The page sets its own Content-Security-Policy in a `<meta>` tag, so no server configuration is needed.
