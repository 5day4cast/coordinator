# Owed winners: paying a winner the Lightning payout missed

A winner is paid over Lightning while that is safe: until 13 blocks before they could claim their
split output on chain, which is `delta` blocks after the split transaction confirms. A payout
that has not succeeded by then, because the coordinator was down or the winner's Lightning
Address failed, leaves the winner **owed**.

The coordinator does not take an owed winner's share on its own. It records the winner, logs
`is owed ... sats; their Lightning payout window closed unpaid`, and leaves their split output on
chain. It sweeps the output to its own key only once an operator approves, and the winner stays
owed until an operator records paying them.

## Watch

- `coordinator_winners_owed` counts winners still owed; alert when it is above 0.
  `coordinator_winners_owed_sat` is what they are owed, and `coordinator_winner_sweeps_held` how
  many outputs wait for approval.
- The operations page shows a notice with each competition that owes a winner. The competition's
  page lists its owed winners with their actions.
- `coordinator admin owed-winners list` shows the same from a terminal.

## Pay an owed winner

Until the reclaim delay passes (twice `delta` after the split), only the winner can spend their
output, through their win path. After it, both they and the coordinator can.

1. **Approve the sweep** on the competition's page, or:

   ```sh
   coordinator admin owed-winners approve-sweep <entry-id>
   ```

   The coordinator broadcasts the sweep once the reclaim delay has passed, at a fee that confirms
   within about a day. The list shows `Swept to the coordinator` once it is out. Sweep before
   paying: a winner paid while their output is still on chain could also claim it.
2. **Pay the winner** their amount from the list, to the Lightning Address their entry authorized
   or another way agreed with them. Keep the payment hash.
3. **Record the payment** on the competition's page, or:

   ```sh
   coordinator admin owed-winners settle <entry-id> --note "<payment hash>"
   ```

   Recording a payment also approves the sweep, if it was not approved yet.

A winner who claims their output on chain first is no longer owed: the coordinator finds the
output spent when it tries the sweep, records the claim, and shows `Claimed on chain`. Pay
nothing in that case.

The competition completes once every winner's output is settled: closed after a Lightning
payout, swept, claimed by the winner, or too small to sweep.
