# Coordinator enclave and relay

Coordinator owns the DLC, invoice, and Lightning Address verifier inside its custom enclave image.
The coordinator service itself does not run inside an enclave.
`coordinator-verifier-enclave` is Keymeld's enclave binary with the coordinator's verifier compiled in, deployed in place of the stock `keymeld-enclave` beside an unmodified Keymeld gateway.
Keymeld provides generic authorization, custody, confidential transport, and MuSig2 operations.
The standard Keymeld image does not register the Coordinator verifier.

| Component | Responsibility |
| --- | --- |
| `coordinator-escrow` | Participant policy, application payloads, DLC checks, and invoice validation. |
| `coordinator-escrow-verifier` | Trusted rules and optional enclave HTTPS. |
| `coordinator-verifier-enclave` | Static verifier registration and generic Keymeld runtime. |
| `coordinator-lnurl-relay` | Bounded host relay for DNS hints and encrypted TLS traffic. |

The application receives encrypted results through Keymeld's confidential transport.
The Keymeld gateway does not receive application policy, binding, invoice, or payment evidence in plaintext.
The relay sees network destinations and traffic metadata. TLS authentication and HTTP parsing remain inside the enclave.

## Consent and recovery

New entries require a fresh participant signature over the generic escrow policy.
The policy includes separate contract-signing, preimage-release, and signing-key-release permissions.
The signing-key and preimage recipients use the accepted contract's market-maker public key.
Old payout signatures do not authorize the new policy.

Each settlement candidate fixes its invoice under the preimage-release permission.
The key-release preparation requires that first authenticated preparation receipt.
Both preparations use the same attempt and exact application parameters.
Persist both receipts before payment. Use the same payment proof to execute both permissions.

Prepared receipts support payment reconciliation after restart and after LNURL is disabled.
Recovery does not request another invoice.
Recovery from an authenticated successful-execution receipt restores the verifier's claim decision before Keymeld returns the recovered release.
Both release grants contain explicit participant consent for renewable preparation and single-use execution.
After payment reconciliation confirms that an invoice was not paid, the worker can request a new candidate.
Each renewal supplies the authenticated predecessor receipt for that permission and retains the same release action and recipient.
The application verifier checks the contract, amount, network, and recipient invoice again.
Earlier candidate receipts remain valid for a late payment proof until one candidate executes.
The first successful execution fixes the winning candidate; another candidate cannot execute the same grant.
The current protocol bounds each permission to 16 prepared candidates.
Keep earlier prepared receipts and payment records through reconciliation. A newer invoice does not invalidate an earlier payment proof.
Sealed receipts do not provide global rollback protection.
The application must retain its durable payment journal and unique payment-hash constraints.

## Upgrade active deployments

The confidential Coordinator service requires its encrypted `keymeld_protocol_state` checkpoint.
A legacy gateway-managed session record cannot be reinterpreted as this checkpoint or as fresh v2 participant consent.
This release does not transparently migrate active keygen or signing sessions.

Before switching, inventory active sessions and retain their database records and existing recovery tools.
Complete them under the existing protocol, or safely cancel and refund sessions that have not committed funds.
Keep the previous recovery mechanism available for completed funded contracts until settlement finishes.
Do not delete old custody or contract data during the upgrade.
Validate the migration and funded recovery procedure before releasing the new deployment.

## Build and runtime choices

| Nix package | Compiled behavior |
| --- | --- |
| `coordinator-verifier-enclave` | Invoice fallback and prepared-payment recovery. |
| `coordinator-verifier-enclave-lnurl` | Also includes Lightning Address resolution. |
| `coordinator-lnurl-relay` | Separate host process for enclave network access. |

The `lnurl` Cargo feature is disabled by default.
`COORDINATOR_ESCROW_LNURL_ENABLED` defaults to `false` and accepts only `true` or `false`.
Enabling it without compiled LNURL support fails startup.
`COORDINATOR_LNURL_RELAY_PORT` defaults to `8101`.
Production uses VSock. Local TCP requires explicit unattested development mode.

For the local simulated stack:

```sh
COORDINATOR_ESCROW_LNURL_ENABLED=true run-keymeld
```

The launcher starts the Coordinator enclave and a separate relay.
The gateway configuration contains no application capability or LNURL settings.
The authorized client checks verifier capabilities through confidential transport.

## Build a measured image

The EIF helper builds an enclave image file and a JSON measurement manifest.
It does not publish artifacts, update trusted measurements, or deploy an enclave.

Set the provisioned gateway verification key, immutable KMS key ARN, AWS region, and enclave identifier.
Then select the compiled variant and runtime toggle:

```sh
export COORDINATOR_ESCROW_VARIANT=lnurl
export COORDINATOR_ESCROW_LNURL_ENABLED=true
export COORDINATOR_LNURL_RELAY_PORT=8101
bash scripts/build-coordinator-verifier-enclave-eif.sh
```

`ENCLAVE_GATEWAY_PUBLIC_KEY`, `ENCLAVE_KMS_KEY_ID`, and `AWS_REGION` are required.
Use `COORDINATOR_ESCROW_VARIANT=invoice` for an image without LNURL support.
The helper refuses existing output paths.
Optional EIF signing uses `EIF_SIGNING_KEY` and `EIF_SIGNING_CERTIFICATE`.
Signing credentials go only to Nitro CLI.

Review the resulting image hash, PCR measurements, verifier identity, source revision, and runtime settings.
Update accepted attestation measurements and KMS policy for the reviewed custom image.
Do not reuse measurements from the standard Keymeld enclave.

The relay can run as a separate host service.
An example unit is [coordinator-lnurl-relay.service](../deploy/enclave/coordinator-lnurl-relay.service).
Install the reviewed binary at the unit's configured path before enabling it.
Keep the relay port consistent with the measured enclave configuration.

## Verification status

The build-helper regression tests mock Nix, Docker, and Nitro CLI.
They check variant selection, measured runtime settings, and invalid configuration rejection.
They do not validate an actual EIF or hardware attestation.

The complete release still requires the funded browser-offline payout acceptance test and custom-image validation on Nitro hardware.

## Other applications, including Ark

The generic policy permits an application verifier to derive exact signing actions from authenticated transaction artifacts.
An Ark verifier could interpret its transaction tree, ownership proofs, commitments, and protocol phase inside the enclave.
It must reconstruct each sighash from those artifacts before approving the exact MuSig2 plan.
A host-provided digest or approval flag cannot replace that verification.

Key ownership, immutable policy selection, and recipient restrictions belong to the generic custody boundary.
Transaction semantics belong to the registered application verifier.
The Coordinator verifier follows this split for DLC contracts and settlement evidence.

The signing grant explicitly permits repeated signing of the identical approved scope.
A retry uses fresh MuSig2 nonce state and a new signing-session identifier.
Release execution remains single-use, with exact retries for recovery.
Release preparation is separately renewable only when participant consent explicitly permits it; the underlying release action cannot change.
Different applications can require different replay rules, including legitimate repeated forfeit signing.

These additional interfaces are not implemented by this Coordinator extraction:

- MuSig2 interaction with signing parties outside the managed Keymeld participant set.
- A general plain BIP340 signing interface governed by escrow permissions.
- Immutable application policy attached to every generic stored key outside session escrow.
- Durable anti-equivocation state anchored independently of host-controlled sealed receipts.

Sealed receipts authenticate earlier decisions. They do not prevent a host from replaying an older complete state snapshot.
An application that requires cross-restart uniqueness needs an independent durable state anchor.
The verifier execution hook is asynchronous and runs before custody effects, outside the session and escrow ledger locks.
This permits a future application-owned witness or compare-and-set commit. No such witness is implemented here.
