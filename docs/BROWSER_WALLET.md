# Browser wallet and login keys

The browser holds each player's keys for the ticketed DLC
([dlctix](https://github.com/tee8z/dlctix),
[protocol](https://conduition.io/scriptless/ticketed-dlc/)). The code lives in
`crates/coordinator-wasm`. This document records its trust boundaries and the
invariants the code enforces.

## Trust boundary

The coordinator is the market maker, a counterparty. Everything it sends is
untrusted: contract parameters, PSBTs, aggregate nonces, Keymeld assignments.
Entry keys reach JavaScript only through the explicit legacy recovery path.
Automatic and signed-invoice payouts keep entry secrets inside WASM and the attested enclave until payment proof authorizes release.

## Keys

| Material | Derivation | Leaves the browser |
| --- | --- | --- |
| Wallet seed | 32 random bytes | NIP-44 encrypted to the user's own Nostr key, stored by the coordinator |
| Entry key | `tagged_hash("coordinator/entry-key/v1", seed ‖ network magic ‖ entry id)` | Encrypted to an attested Keymeld enclave; released to the coordinator after verified payment |
| Payout preimage | `tagged_hash("coordinator/payout-preimage/v1", seed ‖ network magic ‖ entry id)` | Encrypted with the authorized payout policy; released after verified payment |

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
are not used. Either way the coordinator, as market maker, verifies the
complete aggregated signature set (`TicketedDLC::into_signed_contract`) before
it accepts the signed contract and funds it.

## Funding PSBT (escrow mode)

Before signing, the wallet checks all of the following:

- The transaction's txid is the accepted contract's funding outpoint.
- The contract's funding output is at that vout.
- Each signed input's witness script hashes to its P2WSH prevout and pushes the
  entry key.
- The sighash is `ALL`.

It errors if no input belongs to the entry.

The coordinator builds and broadcasts the escrow transaction only after the
ticket's HODL invoice is accepted, never hands it out beforehand, and its
output carries a coordinator reclaim branch (`infra/escrow.rs`) so an escrow
nobody spends is recovered once a paying user's refund window has passed.

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

## Payout authorization

[Automatic payout escrow](PAYOUT_ESCROW.md) binds an entry's recipient and payout rules before ticket payment.
The browser checks its selected address, ticket, entry key, payout hash, and economic terms before encrypting the registration.
The actual funding contract is bound inside Keymeld before signing.

An ordinary invoice fallback signs the exact invoice and completed contract context.
It never sends entry secrets before payment.
The browser does not silently downgrade to legacy recovery when an authorization lookup fails.

## Remaining trust limits

- Legacy recovery requires explicit consent to release entry secrets before payment.
  Existing entries cannot acquire an escrow policy without their owner's authorization.
- After a paid sellback, the coordinator stores the purchased secrets in its database to complete settlement.
- The complete contract does not exist before all entrants register.
  Players authorize fixed economics first; Keymeld enforces those terms before signing the eventual contract.
- The LNURL provider controls invoice secrets.
  A colluding provider can disclose a preimage without receiving a payment.
- Enclave receipts support restart recovery but cannot prevent hostile rollback without an external monotonic state service.
