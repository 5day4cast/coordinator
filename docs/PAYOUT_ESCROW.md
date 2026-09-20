# Enclave-escrowed payouts

A winner's on-chain share of a ticketed DLC is guarded by their
`payout_hash`. The market maker (this coordinator) can sweep that share only
with the payout preimage, which the winner sells after being paid off-chain.
The trusted flow hands the preimage and entry key over first and relies on
the coordinator to pay. This flow keeps the preimage inside the Keymeld
enclave that already holds the entry key, and lets the coordinator obtain it
only by proving it paid the winner's Lightning Address. Keymeld's side is
described in its `docs/PAYOUT_ESCROW.md`; this page is the coordinator's.

## Entry

1. The account has a Lightning Address (required at signup).
2. On entry, the browser fetches the address's LNURL pay request and a probe
   invoice **directly from the provider** (`entries.js` `probePayoutPolicy`),
   decodes the invoice, and takes the payee node key from it. The probe
   invoice is never paid. Providers that block cross-origin requests make
   the entry fall back to the trusted payout.
3. The wallet seals `PayoutPolicy { lightning_address, payee_node_id }` in
   the Keymeld registration envelope next to the entry key
   (`keymeldRegistration(entry_id, assignment, policy)`), and the entry body
   carries the same policy in the clear.
4. The coordinator validates the clear policy (address equals the account's,
   node key well formed), stores it on the entry, and registers the
   participant with Keymeld stating that policy as the expected one. The
   enclave rejects the registration unless the sealed policy matches, so an
   accepted entry's policy is exactly what the enclave will honour.

The wallet's payout preimage is `derive_payout_preimage(entry_secret)`, the
same tagged hash the enclave computes, so `payout_hash` commits to a value
both sides can produce and nothing new has to be stored anywhere.

## Signing

After Keymeld signs the contract, the coordinator stores the
`SigningReceipt` (the authorized batch and the authority's signature) on the
competition. A payout claim must present the batch exactly as signed; the
enclave rebuilds the contract from `ContractParameters` and the funding
outpoint and checks that the signed sighashes are precisely that contract's.

## Claim

`POST /api/v1/competitions/{id}/entries/{id}/claim` with only `ticket_id`
(the NIP-98 signature proves the entry's owner). The coordinator:

1. verifies the entry won and has not been paid (`verify_payout_release`);
2. resolves the address over LNURL-pay (`infra/lnurl.rs`), requires it to be
   the sealed address, fetches an invoice for exactly the winnings, and
   checks the invoice's recovered payee key is the sealed node key;
3. records the pending payout with the LNURL metadata and address, then pays.

Entries without a sealed policy use the trusted claim, which sends the
preimage and entry key with the request as before.

## Release

When the payment settles, LND reports the preimage and the payout row keeps
it. The payout watcher then asks the enclave to release the payout preimage
(`Keymeld::release_payout_preimage`) with the signing receipt, the contract
commitment, the oracle attestation and the payment proof (invoice, metadata,
preimage). The released preimage is checked against the entry's
`payout_hash` and stored on the entry. Settlement then uses the **sellback**
spending path (market maker signature + payout preimage) for that entry,
since the entry key is never revealed; entries paid through the trusted flow
keep using the cheaper key-path closes.

## What this does not cover

- The enclave's proof of payment is "a preimage of an invoice signed by the
  sealed node"; a provider colluding with the coordinator could hand out
  preimages. The participant chose the provider.
- Providers that issue invoices from several nodes make the release fail
  closed; the payout was still made, and the coordinator sweeps after the
  reclaim delay instead of via the sellback path.
