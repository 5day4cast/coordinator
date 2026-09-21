# Queued competitions on Arkade

Status: design draft. Nothing here is implemented.
Items marked "Pending" still need confirmation against arkd and Keymeld.

Players join a queue until registration closes.
Each buy-in is swapped from Lightning into an escrow VTXO on Arkade within seconds.
At kickoff the coordinator splits the paid tickets into even pools.
One Arkade batch spends the escrow VTXOs into the on-chain funding outputs of the pools' ticketed DLCs.
The pot is the players' own money, so the coordinator fronts no contract value.
If a competition never kicks off, the escrow's refund path returns each player's funds to the player's Lightning Address.

## Why

The first entries against 2.0.0 on Mutinynet exposed these limits of the current model:

- The ticket count, the Keymeld session, and every subset definition are fixed when a competition is created.
  A competition that does not sell out has a session that expects tickets nobody bought.
- A ticket's HODL invoice stays held until the competition fills and funds.
  The coordinator sets no final CLTV, so LND uses its default of 80 blocks.
  That is about 13 hours on mainnet and about 40 minutes on Mutinynet.
  LND cancels a held HTLC 12 blocks before it expires.
- Every held HTLC uses one of at most 483 slots on every channel along its payment path.
- The coordinator funds each contract from its own on-chain wallet before it receives the buy-ins.
- The per-ticket escrow mode spends about 275 vB per entry.
  At 20 sat/vB that is more than a 5,000 sat entry.

## Goals

- Players pay once over Lightning, and the payment completes within seconds.
- Entries cause no on-chain transactions.
- The coordinator fronts no contract value.
  The Ark operator provides on-chain liquidity at kickoff, in exchange for the escrow VTXOs.
- Pool sizes follow the actual demand, and pools are as even as possible.
- One Arkade batch funds every pool of a kickoff.
- A player whose competition never kicks off gets the funds back without running an Ark wallet.

## Non-goals

- Pull payments.
  Too few wallets support Nostr Wallet Connect.
- Keeping HODL invoices open until funding.
  That design is covered under "Alternatives considered".

## Trust model

- **Keymeld enclave.**
  It holds each player's deposited key.
  It signs the escrow's funding path and the pool's DLC only when they match the player's signed template.
  This is the same trust the system already places in Keymeld for unattended DLC signing and automatic payouts.
- **Ark operator.**
  It co-signs collaborative spends and provides liquidity at batch time.
  Until a batch settles, players trust the operator not to double-spend a preconfirmed VTXO.
  The operator cannot take an escrow on its own, because every path also needs the player's key or the player's refund conditions.
- **The player.**
  The player holds their own refund path, which becomes spendable at an absolute locktime `T` and is enforced by script.
  A unilateral exit through the VTXO tree remains the backstop if the operator stops cooperating.

## Terms

- **Competition**: what players see: stations, observation window, entry fee, coordinator fee, and registration close.
  It owns registration and kickoff.
- **Pool**: one ticketed DLC with its own oracle event, Keymeld session, and funding output.
  A pool runs today's competition lifecycle from event creation onward.
- **Ticket**: one paid entry.
  It belongs to a competition at purchase and to a pool from kickoff.
- **Escrow VTXO**: one player's buy-in on Arkade, locked to the escrow script.
- **Key deposit**: an entry key encrypted to the Keymeld enclave at entry.
  It is bound to the competition, the ticket, and the player's signed template, and not to any session.
- **Template**: the competition rules a player signs at entry.
  The enclave checks each concrete pool contract and funding batch against it.
- **Kickoff**: forming pools and funding them in an Arkade batch after registration closes.
- **Ark operator**: whoever runs the arkd server.

## Arkade parameters

The test deployment is Ark Labs' Mutinynet instance, `https://mutinynet.arkade.sh`, which runs arkd (`https://github.com/arkade-os/arkd`).
Its `GET /v1/info` returned the following on 2026-09-21:

| Field | Value | Use |
| --- | --- | --- |
| `signerPubkey` | `03301078808e4f7bc0dadfe29e34b1df8eaf0108ef06b1722274075ebc107a127a` | Server key in the collaborative leaves |
| `sessionDuration` | `60` | Length of a batch session in seconds. The kickoff's post-txid signing must fit inside it. |
| `unilateralExitDelay` | `2048` | Minimum CSV delay on an exit leaf, in seconds. arkd reads values of 512 or more as seconds. |
| `boardingExitDelay` | `604672` | Exit delay for on-chain boarding |
| `fees` | all `0` | Mutinynet only. The mainnet operator fees are pending. |
| `dust`, `vtxoMinAmount` | `330`, `1` | Bounds on output values |
| `maxTxWeight` | `40000` | Bounds the escrow inputs and pool outputs in one intent |

The coordinator reads these values at startup and never hard-codes them.

arkd accepts block-based timelocks only when its VTXO tree expiry is in blocks.
Mutinynet's is in seconds, so escrow timelocks must be timestamps and second-based delays.
Timestamps and seconds are accepted by every arkd deployment.

## Escrow VTXO

Each ticket's escrow follows the arkd script pattern used by ArkLabsHQ/arkade-escrow (`server/src/ark/escrow.ts`).
Every spend condition has a collaborative leaf that includes the server key and a unilateral leaf that does not.
A unilateral leaf is usable only after the escrow is unrolled on chain, and waits a CSV delay from that point.

| Leaf | Signers | Condition | Use |
| --- | --- | --- | --- |
| Funding, collaborative | player, coordinator, server | none | The kickoff batch |
| Refund, collaborative | player, server | absolute locktime `T` | Normal refund |
| Funding, unilateral | player, coordinator | CSV exit delay | Operator stopped cooperating after kickoff started |
| Refund, unilateral | player | CSV unilateral refund delay | Backstop refund |

- The player key is the deposited entry key.
  Keymeld signs for the player.
  Each ticket has its own entry key, so every escrow address is unique without a nonce leaf.
- `T` is a timestamp after `registration_close_at`, with enough margin to finish kickoff and the batch.
- The exit delay is the server's `unilateralExitDelay`.
- The escrow amount is the ticket price: the entry fee plus the coordinator fee.

arkd refuses `CHECKLOCKTIMEVERIFY`, `CHECKSEQUENCEVERIFY`, and signature opcodes inside a condition script.
So no leaf can require both `T` and a CSV delay.
The unilateral refund leaf waits a longer CSV delay instead.
The delay is computed when the escrow address is issued, as `T` minus the issue time plus the exit delay, rounded up to 512 seconds.
An unrolled escrow cannot confirm before it exists, so the player alone can never spend before `T` plus the exit delay.
The delay is also longer than the exit delay, so the unilateral funding leaf always opens first.
BIP68 caps a relative delay at about 388 days, which bounds how far ahead `T` can be.

`coordinator-ark-escrow` builds these scripts.
Its tests check every leaf, tree, and address against `@arkade-os/sdk`.
They also check every escrow against arkd's own `TapscriptsVtxoScript.Validate`, because the SDK accepts scripts that arkd rejects.
A combined CSV and CLTV leaf is one example.

## Timeline

1. The admin creates a competition with `registration_close_at` and the observation window.
   No tickets, oracle events, Keymeld sessions, or escrows exist yet.
2. During registration a player:
   1. requests a ticket, and the coordinator derives the ticket's escrow address,
   2. deposits the entry key with Keymeld and signs the template,
   3. pays a Lightning invoice that a swap turns into an escrow VTXO at that address, which completes within seconds,
   4. submits the entry's picks.
3. At `registration_close_at` the coordinator starts kickoff:
   1. It collects the tickets whose escrow VTXOs exist and whose deposits and entries are complete.
   2. It forms pools; see "Forming pools".
   3. For each pool it creates the oracle event, submits the entries, has Keymeld form the session and run keygen, and builds the contract.
   4. It completes the first MuSig2 round for every pool, because that round does not depend on the funding txid.
   5. It registers one batch intent per pool.
      The inputs are the pool's escrow VTXOs, spent by their collaborative funding leaves.
      The output is the pool's on-chain funding output.
      The intents can land in the same batch.
   6. When the batch reaches finalization, the commitment transaction is known.
      The coordinator checks that it pays each funding output, and Keymeld signs each pool's refund transaction against `(txid, vout)`.
      Only then do Keymeld and the coordinator sign the escrow forfeits.
   7. The Ark operator broadcasts the commitment transaction.
      Keymeld finishes the pools' remaining DLC signatures.
4. Each pool then follows today's lifecycle: attestation, automatic Lightning payouts, and on-chain resolution.
5. If kickoff does not complete by `T`, each escrow is refunded; see "Refunds".

The observation window must start after kickoff has finished.

## Registration

### Swap into the escrow

The player pays a Lightning invoice.
A Lightning-to-Arkade swap delivers the ticket price into the ticket's escrow address.
The Lightning HTLC is held only until the swap completes.

Boltz reverse swaps can pay a custom-script VTXO directly.
In `ark-client`, `get_ln_invoice_for_address` claims the swap's VHTLC into any Ark address, such as an escrow.
`get_ln_invoice_from_hash` and `claim_vhtlc` keep the preimage outside the client, so Keymeld could generate it.
The preimage could then double as the DLC ticket secret.

Arkade's Mutinynet Boltz, `https://api.boltz.mutinynet.arkade.sh`, returned 404 on every path on 2026-09-21, so this has not run on Mutinynet yet.
Swaps therefore run without a third party, in a separate service, `ark-swapd` (crate `coordinator-ark-swap`).
The coordinator asks it for a swap into an escrow address and shows the player the invoice.
The service takes a hold invoice on its LND node for a preimage it generates.
When the payer's HTLC is held, it pays the escrow from its own Ark wallet.
Once the escrow VTXO exists, it settles the invoice.
If the escrow cannot be paid, it cancels, and the payment fails back.
It checks arkd for an existing escrow payment before any send or cancel, so a crash never pays twice or refunds a funded escrow.
Its wallet is topped up with Arkade VTXOs, for example from `mutinynet.arkade.money` or by boarding on-chain coins.

On 2026-09-21 a 20,000-sat payment from thor to odin funded an escrow this way in about 5 seconds, including routing.
The escrow VTXO was `3f8e6e36b8549d3ab54fe02a549a659b4b8a1060383dff09b4cdaab188fa5b5d:0`.

The coordinator supplies the preimage: it is the ticket preimage, so the swap invoice pays to the ticket hash.
Paying the invoice still reveals the ticket preimage to the player, as a ticket's hold invoice does today.

### Key deposit and template

At entry the browser encrypts the entry key and payout preimage to the attested enclave.
The encryption context names the competition, the ticket, and the template digest.
It names no session or manifest, because neither exists yet.
Nothing is signed for a specific contract at entry.

The template fixes the following:

- Network, competition, ticket hash, and payout hash.
- Ticket price and the escrow script parameters, including `T`.
- Pool rules: the maximum pool size, the minimum pool size, the assignment rule, the winners per pool, and the payout split.
- The oracle public key and the event specification: stations, observation window, and scoring.
- The relative locktime, the funding fee ceiling, and the market maker key.
- The payout method: a Lightning Address, invoice fallback, or both, and consent to release secrets after payment.
- The refund destination: the player's Lightning Address, reached through an Arkade-to-Lightning swap.

Entering the competition is the player's consent to this template, as adding a Lightning Address at sign-up is consent to payouts there.
The wallet builds and signs the template as part of submitting the entry, with no extra screen or checkbox.
The entry form shows one line saying where a refund goes if the competition does not start: the player's Lightning Address.

The template does not fix the player slot, the player count, the funding value, the oracle locking points, or the funding outpoint.
They do not exist until kickoff.
The enclave checks at kickoff that each of these follows from the template.

## Forming pools

Let N be the number of complete tickets at registration close.

- If N is below the minimum pool size, every escrow is refunded.
- Otherwise P = ceil(N / max_pool_size), with max_pool_size at most 25, the current confidential Keymeld limit.
  The first N mod P pools receive floor(N / P) + 1 players, and the others receive floor(N / P).
- One winner per pool is the current limit.
  A pool must have more players than winners.

Players are assigned by a seed that the coordinator cannot choose:

```
seed = SHA256("5day4cast/pool-seed/v1" || competition_id || sorted(ticket_ids) || block_hash(close_height))
```

`close_height` is the first block at or after `registration_close_at`.
A Fisher-Yates shuffle, driven by the seed, orders the tickets and fills the pools in order.
The coordinator publishes the seed inputs and the assignment.
The enclave recomputes the assignment before it forms any session.

## Funding in an Arkade batch

A DLC commits to its funding outpoint, so its signatures cannot exist before the batch's commitment txid is fixed.

arkd runs a batch in phases within `sessionDuration` (see `internal/core/application/round_timing.go`):

1. Registration takes a sixth of the session.
   A batch that selects the intent sends `BatchStarted`, and the coordinator confirms.
2. A batch with new VTXOs has its cosigners sign the VTXO tree.
   A kickoff intent creates no VTXOs, so it skips this phase.
3. `BatchFinalization` carries the commitment transaction, so its txid is first known here.
   The connector tree arrives just before.
4. arkd waits until the session ends for every forfeit.
   If one is missing, the batch fails, and arkd bans the scripts of the unsigned VTXOs for its ban duration.

Work after the txid is known must therefore finish in the rest of the session, which is roughly 20 to 40 seconds on the test deployment.
`coordinator-ark::fund_pool` does exactly this:

1. Check that the commitment transaction pays the pool's funding output, and that the connector tree spends from it.
   Each forfeit also spends a connector, so it is void unless this commitment transaction confirms.
2. Run the `before_forfeits` hook, in which Keymeld signs the pool's refund transaction.
3. Sign one forfeit per escrow, with Keymeld as the player and the coordinator, and submit them.

The refund transaction is signed before any forfeit, so a player's funds never sit in a funding output without a way back.
The outcome and split signatures do not need to be ready before the commitment transaction confirms, so they can follow the batch.
If a check or the hook fails, nothing is forfeited, and every escrow stays spendable.
The ban on the unsigned escrows still applies, so the hook must be reliable.

`fund_pool` is tested against a scripted arkd that checks every intent proof and forfeit signature from the PSBTs alone.
`DlcKickoff` supplies the hook for a dlctix pool.
The intent pays `ContractParameters::funding_output()`, which needs no outpoint.
In the hook, it builds the `TicketedDLC` on the funding outpoint and has a `ContractSigner` sign every transaction.
In production that signer is Keymeld's `sign_dlc_batch`.
It then verifies the whole set as the market maker, and stores the contract before any forfeit.
It refuses a contract without an expiry outcome, and a signature set without the expiry signature.

### In the coordinator

The first integration funds one competition as one pool; splitting a queue into pools comes later.
An Arkade competition moves through the existing lifecycle with these differences:

1. **Ticket.**
   The ticket request fixes the ticket's escrow for its entry key and returns it in the payout policy as `ark_escrow`.
   Its refund time `T` is the observation start plus `refund_after_start_secs`, capped at the contract's expiry.
   The invoice comes from `ark-swapd`, for the ticket's hash.
   A worker polls the swaps; a ticket is paid once its escrow VTXO exists, and `ark-swapd` has settled the invoice by then.
2. **Contract.**
   The contract is built without a wallet PSBT and no funding outpoint.
   Keymeld keygen runs as it does today.
3. **Kickoff**, in `AwaitingSignatures`.
   The coordinator binds the contract with a null funding outpoint and runs `fund_pool` with Keymeld as every player's signer.
   The batch pays the funding output, plus the coordinator's fee when it is at least dust.
   The coordinator stores the batch's commitment transaction, then moves the competition straight to `FundingBroadcasted`.
   A failed batch spends nothing and is retried; after repeated failures the competition fails.
   If the process stops after the batch but before saving the competition, the next attempt finds the stored commitment and has Keymeld sign the contract again.
4. **After funding.**
   Confirmation, attestation, and outcome transactions are unchanged; the commitment transaction is the funding transaction.
   Payout preparation presents the commitment transaction to Keymeld, since the bound contract has no outpoint.
   The coordinator never settles an Arkade ticket's invoice.

The browser checks the escrow without asking the player anything more.
Its player key must be the entry key and its coordinator key the pool's market maker.
It must refund no later than the contract's expiry, and its fee may not exceed the ticket price above the player's share of the pool.

Gaps:

- A cancelled Arkade competition does not refund its escrows yet; see [Refunds](#refunds).
- The competition handler runs competitions one at a time, so a kickoff holds up the others for a batch or two.
- If the process stops inside a batch that then completes, the next attempt fails on spent escrows until an operator records the commitment.

### Live run on Mutinynet, 2026-09-21

`crates/coordinator-ark/examples/mutinynet_kickoff.rs` ran the whole path against `https://mutinynet.arkade.sh`.
It used three players with 20,000-sat escrows, and local keys in place of Keymeld.

| Step | Result |
| --- | --- |
| Pay three escrow addresses from an Ark wallet | Ark transaction `ca05c9ff751bfafff6d9d670e60be07ab9ba0c9bb108e988421253a9fc8dc797`, three preconfirmed escrow VTXOs |
| Register the kickoff intent, then wait for a batch to select it | Batch `fa62120a-7f1c-416f-878c-8216005ccd6b` |
| Sign the contract in the hook: 3 outcome and 6 split transactions, plus expiry | 0.13 s with local keys |
| Forfeit the escrows, then wait for finalization | 38.1 s from registration to finalization |
| Commitment transaction | `69a28469d257a111660943e728da850bb571513d864e1b33d7261e6de458c608`, output 0 pays 60,000 sats to the dlctix funding script, confirmed at height 3444796 |
| Broadcast the pre-signed expiry transaction at its height | `25f3b890d70be5d7a5ae32b5e68be72713a0222e9d32ac40eb4aee4fc56d26b5`, spends the funding output, confirmed at height 3444801 |

Mutinynet's fees were zero, so the funding output held the escrows' full value.
The escrow VTXOs expired seven days after they were created.
Kickoff must therefore run within seven days of the first entry, or the escrows must be renewed first.

A second run had Keymeld sign as every player, with escrows funded over Lightning through `ark-swapd`.
It is the ignored test `keymeld_kicks_off_a_pool_on_mutinynet` in `crates/coordinator`.
Keymeld ran in process with the real confidential transport and enclave logic.

| Step | Result |
| --- | --- |
| Three 2,000-sat swaps paid from thor, each settled once its escrow VTXO existed | `46fef6a2…:0`, `4259662c…:0`, `9975f3b4…:0` |
| Keymeld signs the intent proof, 4 signatures | 1.56 s |
| Keymeld signs the contract, expiry included, after the verifier checks the commitment transaction | 2.45 s |
| Keymeld signs the 3 forfeits, after the verifier checks the connectors and the signed contract | 1.68 s |
| Batch `ab4f9c29-d52b-44e8-9540-ab62a85ea7b2`, registration to finalization | 33.7 s |
| Commitment transaction | `2c947b2efc685dd9de0ea5261d1cb4e5b838649c10fbb2eb4948e982f45fbe6d`, output 0 pays 6,000 sats to the dlctix funding script, confirmed at height 3445225 |
| Broadcast Keymeld's expiry transaction at its height | `1d1ded1347f1925655a25f16c543b0b1fd6d5fe044b5ed8104a1c55d24bac5b1`, confirmed at height 3445229 |

Pending: Mutinynet's ban duration for missing forfeits.

If one pool cannot be signed in time, the coordinator retries the kickoff in the next batch with the same pools.
If a pool fails again, its tickets are refunded, and the other pools are funded in a later batch.

## Kickoff latency

A two-player pool on Mutinynet on 2026-09-21 took:

| Step | Duration |
| --- | --- |
| Keymeld keygen | 0.9 s |
| Wait for the next lifecycle tick | 60 s |
| Keymeld signing, 3 outcome and 6 split signatures | 2.6 s |
| Funding broadcast | 0.7 s |

Most of the elapsed time was the coordinator's 60-second tick.
Kickoff should start each step as soon as the previous one completes.

Confidential signing sends each step through `/api/v1/confidential` as a synchronous request.
It does not use the gateway's session loop or the SDK's backoff polling.
The 2.6 s is therefore most likely network round trips and durable session writes.

These changes can shorten signing:

- Instrument each round of `sign_dlc_batch` to find where the time goes.
- Contact all enclaves in parallel within each MuSig2 round.
- Batch the per-step session writes where crash safety allows.
- Move the first MuSig2 round before the batch.
  Pre-generated nonces must be discarded if the funding transaction changes, because reusing a nonce reveals the key.

A 25-player pool with one winner needs about 100 signatures.
The time after the txid is known must still be measured.

## Refunds

After `T`, a ticket that no pool funded is refunded through the collaborative refund leaf.
The player's template authorizes Keymeld to spend that leaf into an Arkade-to-Lightning swap that pays the player's Lightning Address.
A refund therefore needs no action from the player and no Ark wallet.
If the Lightning Address fails, the refund is retried, and the player can claim through the invoice fallback.

If the operator stops cooperating, the player can unroll the escrow and spend the unilateral refund leaf.
That leaf opens no earlier than `T` plus the exit delay.

Pending: the cost of a unilateral exit on this deployment, which depends on the VTXO tree depth and the escrow's ancestry.

## Keymeld changes

Keymeld 0.5.0 already covers the contract side.
Its generic escrow engine holds participant keys under participant-signed policies.
Trusted verifiers compiled into the measured enclave, such as the Coordinator's DLC verifier, authorize each action.
`bind_payout_contract` commits a contract and its funding outpoint to every accepted policy, and `sign_dlc_batch` signs the whole contract with MuSig2.
The coordinator now calls both from the kickoff hook through `KeymeldPoolSigner`, once the batch fixes the funding outpoint.

The escrow side needs a new primitive.
An escrow's funding leaf checks the player's key with its own `CHECKSIG`.
Spending it needs a plain BIP340 signature from the entry key, not a MuSig2 aggregate.
Keymeld's escrow documentation lists plain BIP340 permissions among the primitives still to be added.

The change, now built on Keymeld branch `feat/escrow-bip340-signing`, adds a `SignBip340` permission.
It signs 32-byte digests, such as taproot script-path sighashes, with the participant's untweaked key.
A `VerifierAuthorizedAttempts` repetition lets each fresh attempt sign once, because a retried batch signs new transactions.

The original recommendation was a `Sign` permission scope for one BIP340 script-path signature.
The scope fixes the key, the exact sighash, and the transaction it belongs to.
A Coordinator Ark verifier in the measured enclave authorizes each one:

- An intent proof must spend only escrows of the player's pool.
  Its outputs must be the pool's funding output and the permitted coordinator fee.
- A forfeit must spend the escrow and a connector from a commitment transaction that pays the pool's funding output.
  Keymeld must already have bound and signed that pool's contract, including the expiry transaction.
- A refund, after `T`, must pay only into the swap toward the template's refund destination.

The coordinator side is also built.
A ticket with an Arkade escrow carries an `ark_escrow` consent: the escrow's tap tree, and a cap on the coordinator's fee per escrow.
The Coordinator verifier then requires a `sign_ark_escrow` grant.
It checks that the escrow's player key is the deposited entry key, and that its coordinator key is the market maker.

Keymeld allows one binding per participant per keygen session, so an Arkade pool is bound once, before the batch, with a null funding outpoint.
Every later step presents the batch's commitment transaction, and the verifier checks that it pays the funding output at the named index.
Those steps are the contract signing, the forfeits, and payout settlement.
The verifier authorizes an intent proof only if it spends this escrow through its funding leaf, into the bound contract's funding output and a capped fee.
It authorizes a forfeit only if the forfeit spends this escrow and a connector descending from that commitment transaction.
The contract must also be completely signed at that outpoint, expiry transaction included.

An in-process test runs the real Keymeld enclave logic and transport with the Coordinator verifier.
Keymeld signs every player's intent-proof inputs, the whole contract, and every forfeit, and a scripted arkd checks every signature.
In release builds:

| Players | Intent proof | Contract | Forfeits | Inside the batch window |
| --- | --- | --- | --- | --- |
| 3 | 0.1 s | 0.2 s | 0.1 s | 0.4 s |
| 10 | 0.8 s | 2.2 s | 0.9 s | 3.1 s |
| 25 | 4.4 s | 22.4 s | 6.0 s | 28.4 s |

The work inside the window keeps its Keymeld journal in memory.
The coordinator's durable checkpoint rewrites the whole protocol state twice per command, so an earlier run took 156 s for 10 players.
Nothing signed inside a batch needs replaying: a failed batch is retried with a new commitment transaction and fresh attempts.

Contract signing grows faster than linearly: a dlctix contract has about 3n MuSig2 items, each with n+1 signers.
A 25-player pool fits Mutinynet's 60-second session, but tightly.
Moving the first MuSig2 round before the batch, as nonces need no message, would cut this further.

Pending: `sign_contract` repeats only an identical scope, so a pool whose batch fails cannot sign again for a new outpoint in the same keygen session.
Arkade pools need per-attempt repetition for `sign_contract` too.

The alternative is a MuSig2 aggregate of the player and coordinator keys in the funding leaf.
That fits Keymeld's current engine, but each escrow address would then need a keygen session before the player could pay.

The design needs these operations:

1. **Deposit.**
   The enclave stores an encrypted entry key and payout preimage bound to a competition, a ticket, and a signed template.
   Deposits expire and are deleted after `T` and a successful refund.
2. **Form pools.**
   The coordinator sends the competition, the ticket list, the seed inputs, and the assignment.
   The enclave checks that each placed key was deposited for that competition, that each is used once, and that the assignment follows the seed.
   It then creates each pool's session from the deposits and runs keygen.
3. **Template verification.**
   Before signing, the Coordinator verifier checks each pool contract against every member's template.
   The checks cover the player count, the funding value, the payout split, the oracle announcement, the locktimes, and the fee ceiling.
4. **Escrow funding signature.**
   The enclave signs a player's escrow input only in a batch whose outputs are that player's pool funding output and the permitted coordinator fee.
5. **Refund signature.**
   After `T`, the enclave signs the refund leaf only into the swap to the template's refund destination.

## Closing

Pools resolve as today.
Each pool broadcasts its pre-signed outcome transaction.
When all of a pool's winners were paid over Lightning, the coordinator's unified close spends that pool's output.
The final sweeps of all pools of a kickoff go into one transaction.

Pending: whether Keymeld, which holds every player's key, can close a pool's funding output with a cooperative key-path spend after its payouts.
If it can, the outcome transaction is not needed in the happy path.
Pending: whether the closing sweep can board back into Arkade.

## On-chain cost

- Entries: none.
- Kickoff: one funding output per pool, inside a commitment transaction that the Ark operator broadcasts and shares with other users, plus the operator's fee.
- Each pool: one outcome transaction and one input in the shared close transaction.
- Refunds: none on-chain, unless a player exits unilaterally.

## Failure paths

| Failure | Result |
| --- | --- |
| The swap fails before the escrow exists | The Lightning payment fails back. |
| Fewer players than the minimum pool | Every escrow is refunded after `T`. |
| Kickoff cannot complete before `T` | Every escrow is refunded after `T`. |
| A pool cannot be signed within a batch | Kickoff retries in the next batch. Repeated failures refund that pool. |
| Ark operator unavailable before kickoff | Kickoff waits. Players can unroll and exit alone, no earlier than `T` plus the exit delay. |
| After funding | Today's pool lifecycle applies. |

## Implementation phases

1. **Rust arkd client.**
   Started: `coordinator-ark` uses Arkade's `ark-core` and `ark-grpc` 0.11 crates, and drives the kickoff batch.
2. **Escrow script builder.**
   Started: `coordinator-ark-escrow` encodes the leaves exactly as arkd does.
   Its vectors come from the TypeScript SDK and from arkd's own validator.
3. **Swap integration.**
   Lightning into the escrow address runs through `ark-swapd`.
   Pending: a refund VTXO out to a Lightning Address.
4. **Keymeld deposit, pool formation, and escrow and refund signing.**
   Escrow signing is done: Keymeld's `SignBip340` permission and the coordinator verifier's Ark rules.
   Pending: pool formation and refund signing.
5. **Competition and pool split** in the coordinator, with an event-driven kickoff.
   One competition as one Arkade pool is wired; see [In the coordinator](#in-the-coordinator).
6. **Batch funding** with MuSig2 nonces exchanged before the txid.
   Done for Keymeld signing inside the batch.
7. **Browser changes.**
   The browser checks the escrow in the payout policy and shows the refund destination.
   Pending: depositing the key against a template instead of concrete terms.
8. **Synth scenarios.**
   Add scenarios for a full queue, uneven demand, a kickoff failure, a missed batch, and refunds to a Lightning Address.

## Alternatives considered

- **HODL invoices held until funding.**
  This is non-custodial and needs no on-chain escrow.
  It holds HTLC slots across whole payment paths for the length of registration, and it depends on the invoice's final CLTV.
  It also needs the coordinator to front the contract value before settling.
- **Per-ticket on-chain escrow transactions.**
  Each entry costs about 275 vB, which is only reasonable for entries in the hundreds of dollars.
- **Enclave-enforced reserves with pre-signed refunds.**
  A reserve is the coordinator's shared output, locked only by the enclave's refusal to co-sign.
  Each refund is a pre-signed transaction, which needs versioning and is exposed to state rollback.
  Escrow VTXOs give each player their own refund path instead.
- **Fund first, then sell tickets**, the original ticketed-DLC ordering.
  The coordinator would have to size and fund pools without knowing the demand.
- **Pull payments** through Nostr Wallet Connect.
  Too few wallets support them.

## Open questions

- Registration length, `T`, and the lead time before the observation start.
- The mainnet operator fees and the VTXO expiry, and who pays for renewal if kickoff is delayed.
- Whether one winner per pool is enough, or whether raising the confidential signing limit is in scope.
