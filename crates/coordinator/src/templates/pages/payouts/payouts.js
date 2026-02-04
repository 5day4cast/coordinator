class Payouts {
  constructor(coordinator_url, oracle_url) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new window.AuthorizedClient(
      window.nostrClient,
      coordinator_url,
    );
  }

  async getPayableEntries() {
    const [entries, competitions] = await Promise.all([
      this.getUserEntries(),
      this.getCompetitions(),
    ]);

    const payableEntries = await Promise.all(
      entries
        .filter((entry) => !entry.paid_out_at && !entry.payout_ln_invoice)
        .map((entry) => this.checkEntryPayout(entry, entries, competitions)),
    );

    return payableEntries.filter(Boolean);
  }

  async checkEntryPayout(entry, allEntries, competitions) {
    const competition = competitions.find((c) => c.id === entry.event_id);
    if (!competition?.attestation) return null;

    const oracleEvent = await this.getOracleEvent(entry.event_id);
    if (
      !oracleEvent?.attestation ||
      oracleEvent.attestation !== competition.attestation
    )
      return null;

    const playerIndex = competition.contract_parameters.players.findIndex(
      (player) => player.pubkey === entry.ephemeral_pubkey,
    );
    if (playerIndex === -1) return null;

    const outcomeKey = this.getCurrentOutcome(competition);
    if (!outcomeKey) return null;

    const outcomeWeights =
      competition.contract_parameters.outcome_payouts[outcomeKey];
    if (!outcomeWeights) return null;

    const totalWeight = Object.values(outcomeWeights).reduce(
      (a, b) => a + b,
      0,
    );
    const playerWeight = outcomeWeights[playerIndex] || 0;
    if (playerWeight <= 0) return null;

    const payoutAmount =
      (competition.event_submission.total_competition_pool * playerWeight) /
      totalWeight;
    if (payoutAmount <= 0) return null;

    return {
      entry,
      competition,
      entryIndex: allEntries.findIndex((e) => e.id === entry.id),
      payout_amount: payoutAmount,
      weight: playerWeight,
      total_weight: totalWeight,
    };
  }

  async getUserEntries() {
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/entries`,
    );
    if (!response.ok)
      throw new Error(`Failed to get entries: ${response.status}`);
    return response.json();
  }

  async getCompetitions() {
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/competitions`,
    );
    if (!response.ok)
      throw new Error(`Failed to get competitions: ${response.status}`);
    return response.json();
  }

  async getOracleEvent(event_id) {
    const response = await fetch(
      `${this.oracle_url}/oracle/events/${event_id}`,
    );
    if (!response.ok)
      throw new Error(`Failed to get oracle event: ${response.status}`);
    return response.json();
  }

  getCurrentOutcome(competition) {
    if (!competition.attestation || !competition.event_announcement)
      return null;

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

  async submitPayout(
    competitionId,
    entryId,
    ticketId,
    entryIndex,
    encryptedPayoutPreimage,
    invoice,
    payoutAmount,
  ) {
    if (!invoice) throw new Error("Please enter a Lightning invoice");

    this.validateInvoice(invoice, payoutAmount);

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

    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entryId}/payout`,
      {
        ticket_id: ticketId,
        payout_preimage: payoutPreimage,
        ephemeral_private_key: ephemeralPrivateKey,
        ln_invoice: invoice,
      },
    );

    if (!response.ok)
      throw new Error(`Failed to submit payout: ${response.status}`);
  }

  validateInvoice(invoice, expectedAmount) {
    try {
      const decoded = lightningPayReq.decode(invoice);

      if (decoded.timeExpireDate) {
        const currentTime = Math.floor(Date.now() / 1000);
        if (currentTime > decoded.timeExpireDate)
          throw new Error("Invoice has expired");
      }

      if (decoded.satoshis !== null && decoded.satoshis !== undefined) {
        if (decoded.satoshis !== expectedAmount) {
          throw new Error(
            `Invoice amount (${decoded.satoshis} sats) doesn't match expected (${expectedAmount} sats)`,
          );
        }
        return {
          isValid: true,
          hasAmount: true,
          amount: decoded.satoshis,
          type: "fixed-amount",
        };
      }

      return {
        isValid: true,
        hasAmount: false,
        amount: null,
        type: "any-amount",
      };
    } catch (error) {
      throw new Error(`Invalid invoice: ${error.message}`);
    }
  }
}

window.Payouts = Payouts;

// Global payout state for the modal
let currentPayoutData = null;
let payoutsInstance = null;

/**
 * Initialize payouts instance
 */
function initPayouts(coordinatorUrl, oracleUrl) {
  payoutsInstance = new Payouts(coordinatorUrl, oracleUrl);
}

/**
 * Open the payout modal when user clicks "Submit Invoice" button
 */
function openPayoutModal(button) {
  const entryId = button.dataset.entryId;
  const competitionId = button.dataset.competitionId;
  const payoutAmount = parseInt(button.dataset.payoutAmount, 10);

  currentPayoutData = {
    entryId,
    competitionId,
    payoutAmount,
  };

  // Clear previous state
  const invoiceInput = document.getElementById("lightningInvoice");
  const errorDiv = document.getElementById("payoutModalError");
  if (invoiceInput) invoiceInput.value = "";
  if (errorDiv) {
    errorDiv.textContent = "";
    errorDiv.classList.add("hidden");
  }

  // Open modal
  const modal = document.getElementById("payoutModal");
  window.openModal(modal);
}

/**
 * Handle payout invoice submission
 */
async function submitPayoutInvoice() {
  const errorDiv = document.getElementById("payoutModalError");
  const submitBtn = document.getElementById("submitPayoutInvoice");
  const invoice = document.getElementById("lightningInvoice")?.value?.trim();

  if (!invoice) {
    errorDiv.textContent = "Please enter a Lightning invoice";
    errorDiv.classList.remove("hidden");
    return;
  }

  if (!currentPayoutData || !payoutsInstance) {
    errorDiv.textContent = "Payout data not available. Please try again.";
    errorDiv.classList.remove("hidden");
    return;
  }

  submitBtn.disabled = true;
  submitBtn.classList.add("is-loading");
  errorDiv.classList.add("hidden");

  try {
    // Get payable entries to find the entry details
    const payableEntries = await payoutsInstance.getPayableEntries();
    const payableEntry = payableEntries.find(
      (p) => p.entry.id === currentPayoutData.entryId,
    );

    if (!payableEntry) {
      throw new Error("Entry not found or no longer eligible for payout");
    }

    await payoutsInstance.submitPayout(
      currentPayoutData.competitionId,
      currentPayoutData.entryId,
      payableEntry.entry.ticket_id,
      payableEntry.entryIndex,
      payableEntry.entry.payout_preimage_encrypted,
      invoice,
      currentPayoutData.payoutAmount,
    );

    // Success - close modal and refresh the page
    window.closeModal(document.getElementById("payoutModal"));

    // Reload the payouts page to reflect the updated status
    const payoutsLink = document.querySelector('[hx-get="/payouts"]');
    if (payoutsLink) {
      payoutsLink.click();
    } else {
      window.location.reload();
    }
  } catch (error) {
    console.error("Payout submission failed:", error);
    errorDiv.textContent = error.message || "Failed to submit payout";
    errorDiv.classList.remove("hidden");
  } finally {
    submitBtn.disabled = false;
    submitBtn.classList.remove("is-loading");
  }
}

/**
 * Set up payout modal event listeners
 */
function setupPayoutModal() {
  document
    .getElementById("submitPayoutInvoice")
    ?.addEventListener("click", submitPayoutInvoice);

  document
    .getElementById("cancelPayoutModal")
    ?.addEventListener("click", () => {
      window.closeModal(document.getElementById("payoutModal"));
    });
}

window.initPayouts = initPayouts;
window.openPayoutModal = openPayoutModal;
window.submitPayoutInvoice = submitPayoutInvoice;
window.setupPayoutModal = setupPayoutModal;
