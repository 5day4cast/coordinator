# Equal refunds for equal stakes

New contracts use equal relative weights for refund and expiry outcomes. Three equal stakes in a 3,000-sat pool therefore return 1,000 sats each.

Previous contracts rounded refund weights into percentages totaling 100. Three participants received weights 34/33/33, which incorrectly allocated 1,020/990/990 sats.

Ranked winning outcomes retain their existing proportions. The coordinator fee policy is unchanged. Automatic Lightning payouts calculate shares from the full contract pool. Direct on-chain claims also pay the transaction fees defined by the contract.

## Deploy compatible components

Deploy the coordinator, its browser WASM, synth, and rebuilt coordinator verifier enclaves together. The old verifier rejects attested payout tables whose weights do not total 100.

Keep the authorization schema version unchanged. Weight maps already support relative values; the change does not require a new serialized format.

Before accepting new entries, drain competitions with existing entry authorizations. A competition must not mix the old percentage table with the new equal-ratio table. The new browser rejects the old unequal refund table.

Existing signed contracts retain their original payout amounts. Fully enrolled competitions without a built contract retain their previously accepted payout table. Inconsistent or missing authorizations stop contract creation instead of changing agreed economics.

Do not rewrite existing signatures, attestations, or completed payment records. Correcting historical payments requires separate reconciliation.

The sweep-input correction also lets existing close and reclaim jobs progress after deployment.
Reclaiming an unpaid output returns funds to the coordinator. It does not pay the player or clear that payment obligation.

## Verify a new contract

1. Create an unlisted competition with three equal stakes.
2. Confirm refund and expiry weights are `1/1/1` before paying.
3. Exercise the no-reading refund outcome on the test network.
4. Confirm all three Lightning payouts equal one-third of the contract pool.
5. Confirm synth shows the same amounts and no unpaid balance.

Run a separate scored-weather competition to check that ranked payouts retain their original proportions.

## Automated verification

Run the library tests for all components that interpret payout weights:

```sh
cargo test -p coordinator-escrow -p coordinator-escrow-verifier \
  -p coordinator-wasm -p coordinator -p coordinator-synth --lib
```

The regression tests cover equal refund amounts, legacy signed terms, mixed authorizations, invalid ratios, competition capacity, and unchanged ranked payouts. Live entry and payment verification remains separate.

Generate the offline payout fixtures and test them with a Python Playwright environment:

```sh
cargo run -p coordinator --example payout_ui_fixtures -- /tmp/payout-ui
python e2e/browser/payout-ui.py --fixtures /tmp/payout-ui --output /tmp/payout-ui-browser
```

The fixtures use actual templates, bundled assets, and the production security policy. They never connect to a payment service.
The browser checks cover Chromium, Firefox, and WebKit at mobile and desktop widths in both themes.
