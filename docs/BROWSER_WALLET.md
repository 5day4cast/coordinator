# Browser wallet and login keys

The browser holds each player's keys for the ticketed DLC
([dlctix](https://github.com/tee8z/dlctix),
[protocol](https://conduition.io/scriptless/ticketed-dlc/)). The code lives in
`crates/coordinator-wasm`. This document records its trust boundaries and the
invariants the code enforces.

## Trust boundary

The coordinator is the market maker, a counterparty. Everything it sends is
untrusted: contract parameters, PSBTs, aggregate nonces, Keymeld assignments.
Entry keys never reach JavaScript except in the payout sellback described below.

## Keys

| Material | Derivation | Leaves the browser |
| --- | --- | --- |
| Wallet seed | 32 random bytes | NIP-44 encrypted to the user's own Nostr key, stored by the coordinator |
| Entry key | `tagged_hash("coordinator/entry-key/v1", seed ‖ network magic ‖ entry id)` | Encrypted to an attested Keymeld enclave; plaintext only at payout |
| Payout preimage | `tagged_hash("coordinator/payout-preimage/v1", seed ‖ network magic ‖ entry id)` | Plaintext only at payout |

Invariants:

- Keys are bound to the entry UUID, so every entry, and therefore every
  contract, has its own key. The coordinator's database also enforces unique
  `ephemeral_pubkey` and `payout_hash` values.
- The wallet encrypts only to the user's own Nostr key. No API accepts a
  recipient pubkey.
- The payout release re-derives the key from the entry id and refuses to
  release it unless the entry's recorded pubkey matches.
- Key material sits in wrappers that erase on drop. `SecretKey` and `Scalar`
  are `Copy`, so copies made inside the libraries cannot be erased.

## Username and password login

The password never leaves the browser.

```text
stretched = scrypt(password, salt = tagged_hash("login-salt", lowercase(username)))  # N=2^17, r=8, p=1
auth_key  = tagged_hash("login-auth",  stretched)   # sent to the server, stored as argon2(auth_key)
vault_key = tagged_hash("login-vault", stretched)   # stays in WASM
sealed    = XChaCha20-Poly1305(vault_key, nsec, aad = "coordinator/nostr-key/v1")
```

The server can verify a login but cannot open the sealed nsec, so it cannot
decrypt the wallet seed either. Password strength is enforced in the browser
only, because the server never sees the password.

## MuSig2 nonces

Secret nonces are not stored between rounds. The rounds span waiting for the
other players, page reloads, and coordinator restarts, so each round re-derives
the session and must reproduce the public nonces published in round one. The
nonces are therefore deterministic. The seed is a tagged hash of the signing
key, the funding outpoint, and the full `ContractParameters`, so a changed
contract gets fresh nonces.

A deterministic nonce may sign only **one** set of aggregate nonces. Signing
the same aggregate again is harmless: it reproduces the same signature. A
different aggregate gives the other side a second equation in the key, and
three are enough to recover it.

- The browser refuses a second, different aggregate for the same entry. The
  guard lasts for the page session; persist the signed digest per entry before
  wiring this flow into the UI.
- The coordinator accepts each entry's nonces and signatures once: the
  database updates only write into a NULL column, so a repeat is rejected
  even under concurrent requests.

In production, Keymeld runs MuSig2 inside the enclave and the browser rounds
are not used.

## Funding PSBT (escrow mode)

Before signing, the wallet checks all of the following:

- The transaction's txid is the accepted contract's funding outpoint.
- The contract's funding output is at that vout.
- Each signed input's witness script hashes to its P2WSH prevout and pushes the
  entry key.
- The sighash is `ALL`.

It errors if no input belongs to the entry.

## Keymeld enclave trust

Entry keys are encrypted only to a Keymeld enclave whose fresh Nitro
attestation verifies against measurements compiled into the WASM from
`crates/coordinator-wasm/keymeld-trusted-pcrs.json`. The coordinator's ticket
response supplies the slot, session, and gateway. Its `trusted_pcrs` and
`dangerous_trust_unattested_enclaves` are ignored whenever pins are compiled
in, because a coordinator that chose the measurements could substitute its own
enclave. The entry form shows which pins are in use.

- **Pins compiled in:** registration uses only those pins.
- **No pins, mainnet:** registration is refused.
- **No pins, test network:** the coordinator's assignment decides. The entry
  form says so.

Set the pins to Keymeld's reviewed PCR8 (image signer) or PCR0 (image)
measurements when deploying Nitro enclaves. The coordinator serves this WASM,
so the pins only protect users who check that the served
`/ui/pkg/coordinator_wasm_bg.wasm` matches the release build. Each GitHub
release publishes `coordinator-wasm-<version>.sha256` with the SHA-256 of
that module, and the pins it was built from are in the tagged source.

## NIP-98 requests

- Authenticated JSON bodies (`AuthedJson`) require the event's `payload` tag
  to equal the SHA-256 of the exact body bytes. Clients hash the same string
  they send.
- Each auth event id is accepted once (`api::nip98_replay`) for as long as its
  timestamp is within the 60 second window. The guard is per process, which
  matches the single-replica deployment.

## Known gaps

- **The sellback is not atomic.** The payout preimage and entry key are sent
  before the Lightning payout is made (see `PayoutInfo`). A Lightning Address
  can automate payouts but cannot make them atomic, because its invoices use
  the payee's own preimage. The planned fix is an NWC hold invoice whose
  payment hash is `payout_hash`, settled by the browser; Lightning Address
  payouts stay as the automated, trust-the-coordinator option. Until then,
  the database allows one live payout per entry, and the payout row is
  written before the payment is sent.
- **Admin and wallet routes have no authentication.** `/admin`,
  `/api/v1/wallet/*` and `POST /api/v1/competitions` rely on the gateway
  refusing them on the public name.
- **Sold payout secrets are stored in plaintext.** After a sellback the
  coordinator keeps the entry key and preimage it bought, unencrypted, to sign
  the reclaim.
