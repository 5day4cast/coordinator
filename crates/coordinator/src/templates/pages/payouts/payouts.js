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
        .map((entry) => this.checkEntryPayout(entry, competitions)),
    );

    return payableEntries.filter(Boolean);
  }

  async checkEntryPayout(entry, competitions) {
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
      return window.DlcWallet.currentOutcome(
        competition.attestation,
        competition.event_announcement,
      );
    } catch (error) {
      console.error("Failed to determine current outcome:", error);
      return null;
    }
  }

  async submitPayout(competitionId, entry, invoice, payoutAmount) {
    if (!invoice) throw new Error("Please enter a Lightning invoice");

    this.validateInvoice(invoice, payoutAmount);

    // Selling the entry key and payout preimage to the coordinator is the
    // ticketed-DLC sellback. The wallet re-derives the key from the entry id
    // and refuses unless it matches the entry's recorded pubkey.
    const release = window.dlcWallet.payoutRelease(
      entry.id,
      entry.ephemeral_pubkey,
    );

    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entry.id}/payout`,
      {
        ticket_id: entry.ticket_id,
        payout_preimage: release.payout_preimage,
        ephemeral_private_key: release.ephemeral_private_key,
        ln_invoice: invoice,
      },
    );

    if (!response.ok)
      throw new Error(`Failed to submit payout: ${response.status}`);
  }

  /**
   * One-click payout to the Lightning Address on the account. Like the
   * pasted-invoice path this hands the entry key and payout preimage to the
   * coordinator before it pays.
   */
  async claimPayout(competitionId, entry) {
    const release = window.dlcWallet.payoutRelease(
      entry.id,
      entry.ephemeral_pubkey,
    );

    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entry.id}/claim`,
      {
        ticket_id: entry.ticket_id,
        payout_preimage: release.payout_preimage,
        ephemeral_private_key: release.ephemeral_private_key,
      },
    );
    return response.json();
  }

  async saveLightningAddress(address) {
    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/users/lightning-address`,
      { lightning_address: address },
    );
    return response.json();
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
      payableEntry.entry,
      invoice,
      currentPayoutData.payoutAmount,
    );

    // Success - close modal and refresh the page
    window.closeModal(document.getElementById("payoutModal"));
    reloadPayouts();
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
 * The server's error message for a failed AuthorizedClient request.
 */
async function requestErrorMessage(error, fallback) {
  const data = await error.response?.json().catch(() => null);
  return data?.error || error.message || fallback;
}

function reloadPayouts() {
  const payoutsLink = document.querySelector('[hx-get="/payouts"]');
  if (payoutsLink) {
    payoutsLink.click();
  } else {
    window.location.reload();
  }
}

function showPayoutsError(message) {
  const errorDiv = document.getElementById("payoutsError");
  if (!errorDiv) return;
  errorDiv.textContent = message;
  errorDiv.classList.toggle("hidden", !message);
}

/**
 * Handle the "Claim" button: pay the entry's winnings to the account's
 * Lightning Address.
 */
async function claimPayout(button) {
  if (!payoutsInstance) {
    showPayoutsError("Payout data not available. Please try again.");
    return;
  }
  const entryId = button.dataset.entryId;
  const competitionId = button.dataset.competitionId;

  button.disabled = true;
  button.classList.add("is-loading");
  showPayoutsError("");

  try {
    const payableEntries = await payoutsInstance.getPayableEntries();
    const payableEntry = payableEntries.find((p) => p.entry.id === entryId);
    if (!payableEntry) {
      throw new Error("Entry not found or no longer eligible for payout");
    }
    await payoutsInstance.claimPayout(competitionId, payableEntry.entry);
    reloadPayouts();
  } catch (error) {
    console.error("Payout claim failed:", error);
    showPayoutsError(await requestErrorMessage(error, "Failed to claim payout"));
    button.disabled = false;
    button.classList.remove("is-loading");
  }
}

function toggleLightningAddressForm(event) {
  event?.preventDefault();
  document.getElementById("lightningAddressForm")?.classList.toggle("is-hidden");
  document.getElementById("payoutLightningAddress")?.focus();
}

async function saveLightningAddress() {
  const errorElement = document.getElementById("lightningAddressError");
  const button = document.getElementById("saveLightningAddress");
  const address = window.normalizeLightningAddress(
    document.getElementById("payoutLightningAddress")?.value,
  );
  const validationError = window.validateLightningAddress(address);
  if (validationError) {
    if (errorElement) errorElement.textContent = validationError;
    return;
  }
  if (!payoutsInstance) {
    if (errorElement) errorElement.textContent = "Please log in again.";
    return;
  }

  if (errorElement) errorElement.textContent = "";
  button.disabled = true;
  button.classList.add("is-loading");
  try {
    await payoutsInstance.saveLightningAddress(address);
    reloadPayouts();
  } catch (error) {
    console.error("Saving Lightning Address failed:", error);
    if (errorElement)
      errorElement.textContent = await requestErrorMessage(
        error,
        "Failed to save Lightning Address",
      );
  } finally {
    button.disabled = false;
    button.classList.remove("is-loading");
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
window.claimPayout = claimPayout;
window.toggleLightningAddressForm = toggleLightningAddressForm;
window.saveLightningAddress = saveLightningAddress;
window.submitPayoutInvoice = submitPayoutInvoice;
window.setupPayoutModal = setupPayoutModal;
