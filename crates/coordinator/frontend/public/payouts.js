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

            const playerIndex =
              competition.contract_parameters.players.findIndex(
                (player) => player.pubkey === entry.ephemeral_pubkey,
              );

            console.log("Found player index:", playerIndex);
            if (playerIndex === -1) {
              console.log("Player not found in competition");
              return null;
            }

            const outcomeKey = this.getCurrentOutcome(competition);
            console.log("Determined outcome key:", outcomeKey);
            if (!outcomeKey) {
              console.log("Could not determine outcome key");
              return null;
            }

            const outcomeWeights =
              competition.contract_parameters.outcome_payouts[outcomeKey];
            console.log("Outcome weights for", outcomeKey, ":", outcomeWeights);
            if (!outcomeWeights) {
              console.log("No outcome weights found for", outcomeKey);
              return null;
            }

            // Calculate payout amount based on weights
            const totalWeight = Object.values(outcomeWeights).reduce(
              (a, b) => a + b,
              0,
            );
            console.log("Total weight for outcome:", totalWeight);

            const playerWeight = outcomeWeights[playerIndex] || 0;
            console.log(
              "Player weight at index",
              playerIndex,
              ":",
              playerWeight,
            );

            if (playerWeight <= 0) {
              console.log("Player weight is 0 or negative, player didn't win");
              return null;
            }

            const payoutAmount =
              (competition.event_submission.total_competition_pool *
                playerWeight) /
              totalWeight;

            console.log("Calculated payout amount:", payoutAmount);
            console.log(
              "Competition pool:",
              competition.event_submission.total_competition_pool,
            );
            console.log(
              "Calculation:",
              competition.event_submission.total_competition_pool,
              "*",
              playerWeight,
              "/",
              totalWeight,
              "=",
              payoutAmount,
            );

            if (payoutAmount <= 0) {
              console.log("Payout amount is 0 or negative");
              return null;
            }

            console.log(
              "Entry",
              entry.id,
              "is eligible for payout of",
              payoutAmount,
              "sats",
            );

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
      validPayableEntries.forEach((payableEntry) => {
        this.createPayoutRow(payableEntry.entry, entries, {
          payout_amount: payableEntry.payout_amount,
          weight: payableEntry.weight,
          total_weight: payableEntry.total_weight,
        });
      });
    } catch (error) {
      console.error("Error occurred while fetching data:", error);
      this.$error.textContent = `Error loading payouts: ${error.message}`;
      this.$error.classList.remove("hidden");
    }
  }

  async get_user_entries() {
    console.log(
      " Fetching user entries from",
      `${this.coordinator_url}/api/v1/entries`,
    );
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/entries`,
    );
    if (!response.ok) {
      throw new Error(`Failed to get entries: ${response.status}`);
    }
    return response.json();
  }

  async get_competitions() {
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/competitions`,
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

  getCurrentOutcome(competition) {
    if (!competition.attestation || !competition.event_announcement) {
      return null;
    }

    try {
      return window.taprootWallet.getCurrentOutcome(
        competition.attestation,
        competition.event_announcement,
      );
    } catch (error) {
      console.error("Failed to determine current outcome:", error);
      return null;
    }
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
        entry.ticket_id,
        entry.payout_preimage_encrypted,
        parseInt(button.dataset.entryIndex),
        payoutInfo.payout_amount,
      ),
    );

    this.$tbody.appendChild($row);
  }

  async showPayoutModal(
    competitionId,
    entryId,
    ticketId,
    encryptedPayoutPreimage,
    entryIndex,
    payoutAmount,
  ) {
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
          ticketId,
          entryIndex,
          encryptedPayoutPreimage,
          $invoice.value.trim(),
          payoutAmount,
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

  async submitPayout(
    competitionId,
    entryId,
    ticketId,
    entryIndex,
    encryptedPayoutPreimage,
    invoice,
    payoutAmount,
  ) {
    if (!invoice) {
      throw new Error("Please enter a Lightning invoice");
    }
    const validation = this.validateInvoice(invoice, payoutAmount);

    console.log(
      `Invoice validation: ${validation.type} for ${payoutAmount} sats`,
    );

    const nostrPubkey = await window.nostrClient.getPublicKey();

    const payoutPreimage = await window.taprootWallet.decryptKey(
      encryptedPayoutPreimage,
      nostrPubkey,
    );

    const encryptedEphemeralPrivateKey =
      await window.taprootWallet.getEncryptedDlcPrivateKey(
        entryIndex,
        nostrPubkey,
      );

    const ephemeralPrivateKey = await window.taprootWallet.decryptKey(
      encryptedEphemeralPrivateKey,
      nostrPubkey,
    );

    const payoutResponse = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entryId}/payout`,
      {
        ticket_id: ticketId,
        payout_preimage: payoutPreimage,
        ephemeral_private_key: ephemeralPrivateKey,
        ln_invoice: invoice,
      },
    );

    if (!payoutResponse.ok) {
      throw new Error(`Failed to submit payout: ${payoutResponse.status}`);
    }
  }

  validateInvoice(invoice, expectedAmount) {
    try {
      // Use the global lightningPayReq object from bolt11.min.js
      const decoded = lightningPayReq.decode(invoice);

      // Check expiration using bolt11's timeExpireDate field
      if (decoded.timeExpireDate) {
        const currentTime = Math.floor(Date.now() / 1000);
        if (currentTime > decoded.timeExpireDate) {
          throw new Error("Invoice has expired");
        }
      }

      // Check amount - bolt11 provides satoshis directly
      if (decoded.satoshis !== null && decoded.satoshis !== undefined) {
        if (decoded.satoshis !== expectedAmount) {
          throw new Error(
            `Invoice amount (${decoded.satoshis} sats) doesn't match expected payout (${expectedAmount} sats)`,
          );
        }

        return {
          isValid: true,
          hasAmount: true,
          amount: decoded.satoshis,
          type: "fixed-amount",
        };
      } else {
        // Zero-amount invoice (user can pay any amount)
        return {
          isValid: true,
          hasAmount: false,
          amount: null,
          type: "any-amount",
        };
      }
    } catch (error) {
      throw new Error(`Invalid invoice: ${error.message}`);
    }
  }
}
