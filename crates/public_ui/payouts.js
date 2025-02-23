import { AuthorizedClient } from "./authorized_client.js";

export async function displayPayouts(apiBase, oracleBase) {
  const $payoutsTableBody = document.getElementById("payoutsTableBody");
  const $noPayouts = document.getElementById("noPayoutsMessage");
  const $error = document.getElementById("payoutsError");
  const $payoutsContainer = document.getElementById("payouts");

  let payouts = new Payouts(
    apiBase,
    oracleBase,
    $payoutsTableBody,
    $payoutsContainer,
    $noPayouts,
    $error,
  );
  await payouts.init();
}

class Payouts {
  constructor(
    coordinator_url,
    oracle_url,
    $tbody,
    $container,
    $noPayouts,
    $error,
  ) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new AuthorizedClient(window.nostrClient, coordinator_url);
    this.$tbody = $tbody;
    this.$container = $container;
    this.$noPayouts = $noPayouts;
    this.$error = $error;
  }

  async init() {
    try {
      this.$tbody.innerHTML = "";
      this.$noPayouts.classList.add("hidden");
      this.$error.classList.add("hidden");

      // Get entries, competitions, and oracle events
      const [entries, competitions] = await Promise.all([
        this.get_user_entries(),
        this.get_competitions(),
      ]);

      // Filter for potentially payable entries
      const payableEntries = await Promise.all(
        entries
          .filter((entry) => {
            // Must be signed but not paid out
            return (
              entry.signed_at && !entry.paid_out_at && !entry.payout_ln_invoice
            );
          })
          .map(async (entry) => {
            // Find corresponding competition and check for attestation
            const competition = competitions.find(
              (c) => c.id === entry.event_id,
            );
            if (!competition || !competition.attestation) {
              return null;
            }

            // Get oracle event and verify attestation matches
            const oracleEvent = await this.get_oracle_event(entry.event_id);
            if (
              !oracleEvent ||
              !oracleEvent.attestation ||
              oracleEvent.attestation !== competition.attestation
            ) {
              return null;
            }

            // Find this player's index in the contract
            const playerIndex =
              competition.contract_parameters.players.findIndex(
                (player) => player.pubkey === entry.ephemeral_pubkey,
              );
            if (playerIndex === -1) return null;

            // Calculate payout amount based on weights
            const totalWeight = Object.values(
              competition.contract_parameters.outcome_payouts.att0,
            ).reduce((a, b) => a + b, 0);
            const playerWeight =
              competition.contract_parameters.outcome_payouts.att0[
                playerIndex
              ] || 0;
            const payoutAmount =
              (competition.total_competition_pool * playerWeight) / totalWeight;

            return {
              entry,
              competition,
              payout_amount: payoutAmount,
              weight: playerWeight,
              total_weight: totalWeight,
            };
          }),
      );

      const validPayableEntries = payableEntries.filter(Boolean);

      if (validPayableEntries.length === 0) {
        this.$noPayouts.classList.remove("hidden");
        return;
      }

      // Show table and populate it
      validPayableEntries.forEach((payableEntry) =>
        this.createPayoutRow(payableEntry.entry, entries, {
          payout_amount: payableEntry.payout_amount,
          weight: payableEntry.weight,
          total_weight: payableEntry.total_weight,
        }),
      );
    } catch (error) {
      console.error("Error occurred while fetching data:", error);
      this.$error.textContent = `Error loading payouts: ${error.message}`;
      this.$error.classList.remove("hidden");
    }
  }

  async get_user_entries() {
    console.log(
      " Fetching user entries from",
      `${this.coordinator_url}/entries`,
    );
    const response = await this.client.get(`${this.coordinator_url}/entries`);
    if (!response.ok) {
      throw new Error(`Failed to get entries: ${response.status}`);
    }
    return response.json();
  }

  async get_competitions() {
    const response = await this.client.get(
      `${this.coordinator_url}/competitions`,
    );
    if (!response.ok) {
      throw new Error(`Failed to get competitions: ${response.status}`);
    }
    return response.json();
  }

  async get_oracle_event(event_id) {
    const response = await fetch(
      `${this.oracle_url}/oracle/events/${event_id}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to get oracle events: ${response.status}`);
    }
    return response.json();
  }

  createPayoutRow(entry, allEntries, payoutInfo) {
    const $row = document.createElement("tr");
    $row.id = `payout-${entry.id}`;
    $row.classList.add("is-clickable");

    $row.innerHTML = `
        <td>${entry.event_id}</td>
        <td>${entry.id}</td>
        <td>Winnings: (${payoutInfo.payout_amount} sats)</td>
        <td>
          <button class="button is-primary submit-payout"
                  data-competition-id="${entry.event_id}"
                  data-entry-id="${entry.id}"
                  data-entry-index="${allEntries.findIndex((e) => e.id === entry.id)}">
            Submit Invoice
          </button>
        </td>
      `;

    const button = $row.querySelector(".submit-payout");
    button.addEventListener("click", () =>
      this.showPayoutModal(
        button.dataset.competitionId,
        button.dataset.entryId,
        parseInt(button.dataset.entryIndex),
        window.taprootWallet,
      ),
    );

    this.$tbody.appendChild($row);
  }

  async showPayoutModal(competitionId, entryId, entryIndex, wallet) {
    const $modal = document.getElementById("payoutModal");
    const $submitButton = document.getElementById("submitPayoutInvoice");
    const $cancelButton = document.getElementById("cancelPayoutModal");
    const $closeButton = $modal.querySelector(".modal-close");
    const $error = document.getElementById("payoutModalError");
    const $invoice = document.getElementById("lightningInvoice");

    $invoice.value = "";
    $error.classList.add("hidden");
    $modal.classList.add("is-active");

    const closeModal = () => {
      $modal.classList.remove("is-active");
    };

    $cancelButton.onclick = closeModal;
    $closeButton.onclick = closeModal;
    $modal.querySelector(".modal-background").onclick = closeModal;

    $submitButton.onclick = async () => {
      try {
        await this.submitPayout(
          competitionId,
          entryId,
          entryIndex,
          wallet,
          $invoice.value.trim(),
        );
        closeModal();
        this.init(); // Refresh the payouts table
      } catch (error) {
        console.error("Error submitting payout:", error);
        $error.textContent = error.message;
        $error.classList.remove("hidden");
      }
    };
  }

  async submitPayout(competitionId, entryId, entryIndex, wallet, invoice) {
    if (!invoice) {
      throw new Error("Please enter a Lightning invoice");
    }

    const nostrPubkey = await window.nostrClient.getPublicKey();
    const payout_preimage = await wallet.getEncryptedDlcPayoutPreimage(
      entryIndex,
      nostrPubkey,
    );
    const ephemeral_private_key = await wallet.getEncryptedDlcPrivateKey(
      entryIndex,
      nostrPubkey,
    );

    const payoutResponse = await this.client.post(
      `${this.coordinator_url}/competitions/${competitionId}/entries/${entryId}/payout`,
      {
        ticket_id: entryId,
        payout_preimage,
        ephemeral_private_key,
        ln_invoice: invoice,
      },
    );

    if (!payoutResponse.ok) {
      throw new Error(`Failed to submit payout: ${payoutResponse.status}`);
    }
  }
}
