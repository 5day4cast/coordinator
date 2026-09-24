class Payouts {
  constructor(coordinator_url, oracle_url) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
  }

  // Signed by whoever is logged in now: the wallet loads after this object
  // exists, and logging out and in again replaces the signer.
  get client() {
    return new window.AuthorizedClient(session.nostrClient, this.coordinator_url);
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
      return session.wasm.DlcWallet.currentOutcome(
        competition.attestation,
        competition.event_announcement,
      );
    } catch (error) {
      console.error("Failed to determine current outcome:", error);
      return null;
    }
  }

  async submitPayout(competitionId, entry, invoice, payoutAmount, competition) {
    if (!invoice) throw new Error("Please enter a Lightning invoice");
    const endpoint = `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entry.id}/payout-authorization`;
    let response;
    try {
      response = await this.client.get(endpoint);
    } catch (error) {
      if (error.response?.status === 404) {
        throw new Error("This entry has no verified payout authorization. Use the explicit Legacy recovery action only for old entries; this payout action will not release entry secrets.");
      }
      throw error;
    }
    const context = await response.json();
    if (!context.allow_invoice_fallback) throw new Error("Invoice fallback was not authorized for this entry");
    if (context.entry_id !== entry.id || context.competition_id !== competitionId ||
        context.user_id !== entry.ticket_id) {
      throw new Error("Payout authorization belongs to another entry");
    }
    if (!Number.isSafeInteger(context.amount_msat) || context.amount_msat <= 0 || context.amount_msat % 1000 !== 0) {
      throw new Error("Invalid payout amount");
    }
    if (context.amount_msat / 1000 !== payoutAmount) throw new Error("The payout amount changed; refresh this page before authorizing it");
    this.validateInvoice(invoice, payoutAmount);
    if (!competition?.contract_parameters || !competition.funding_outpoint || !competition.signed_contract?.signatures || !competition.attestation) {
      throw new Error("The completed payout contract is unavailable");
    }
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(invoice));
    const invoiceDigest = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    const authorization = session.dlcWallet.authorizePayoutInvoice(JSON.stringify({
      entry_id: entry.id,
      competition_id: competitionId,
      expected_pubkey: entry.ephemeral_pubkey,
      invoice,
      context: {
        keygen_session_id: context.keygen_session_id,
        user_id: context.user_id,
        competition_id: competitionId,
        entry_id: entry.id,
        contract_digest: context.contract_digest,
        amount_msat: context.amount_msat,
        claim_id: crypto.randomUUID(),
        invoice_digest: invoiceDigest,
        expires_at: Math.floor(Date.now() / 1000) + 300,
      },
      contract: {
        contract_parameters: competition.contract_parameters,
        funding_outpoint: competition.funding_outpoint,
      },
      signatures: competition.signed_contract.signatures,
      attestation: competition.attestation,
    }));
    // The exact invoice and entry-key signature are sufficient. The wallet
    // never exports its key or DLC preimage to this browser payout path.
    await this.client.post(endpoint, { invoice, authorization });
  }

  async submitLegacyPayout(competitionId, entry, invoice, payoutAmount, explicitConsent) {
    if (explicitConsent !== true) throw new Error("Legacy recovery requires explicit consent to release entry secrets before payment");
    const endpoint = `${this.coordinator_url}/api/v1/competitions/${competitionId}/entries/${entry.id}`;
    try {
      await this.client.get(`${endpoint}/payout-authorization`);
      throw new Error("This entry uses escrow payouts; use the signed invoice payout instead");
    } catch (error) {
      if (error.response?.status !== 404) throw error;
    }
    this.validateInvoice(invoice, payoutAmount);
    const release = session.dlcWallet.payoutRelease(entry.id, entry.ephemeral_pubkey);
    await this.client.post(`${endpoint}/payout`, {
      ticket_id: entry.ticket_id,
      payout_preimage: release.payout_preimage,
      ephemeral_private_key: release.ephemeral_private_key,
      ln_invoice: invoice,
    });
  }

  async saveLightningAddress(address) {
    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/users/lightning-address`,
      { lightning_address: address },
    );
    return response.json();
  }

  // The wallet decodes the invoice: exact amount, this network, not expired.
  validateInvoice(invoice, expectedAmount) {
    try {
      session.dlcWallet.validateInvoice(invoice, expectedAmount);
    } catch (error) {
      throw new Error(`Invalid invoice: ${error?.message ?? error}`);
    }
  }
}

// Payout state for the modal
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
    legacy: button.dataset.legacy === "true",
  };

  // Clear previous state
  const invoiceInput = document.getElementById("lightningInvoice");
  const errorDiv = document.getElementById("payoutModalError");
  if (invoiceInput) invoiceInput.value = "";
  if (errorDiv) {
    errorDiv.textContent = "";
    errorDiv.classList.add("hidden");
  }

  let warning = document.getElementById("legacyPayoutWarning");
  if (!warning) {
    warning = document.createElement("label");
    warning.id = "legacyPayoutWarning";
    warning.className = "checkbox notification is-warning";
    const approval = document.createElement("input");
    approval.type = "checkbox";
    approval.id = "legacyPayoutApproved";
    warning.appendChild(approval);
    warning.appendChild(document.createTextNode(" I confirm this is a legacy entry. Recovery sends its private key and payout preimage before payment; payment is not guaranteed."));
    errorDiv?.parentNode.insertBefore(warning, errorDiv);
  }
  warning.classList.toggle("is-hidden", !currentPayoutData.legacy);
  const legacyConsent = document.getElementById("legacyPayoutApproved");
  if (legacyConsent) legacyConsent.checked = false;

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

    if (currentPayoutData.legacy) {
      await payoutsInstance.submitLegacyPayout(
        currentPayoutData.competitionId, payableEntry.entry, invoice,
        currentPayoutData.payoutAmount,
        document.getElementById("legacyPayoutApproved")?.checked === true,
      );
    } else {
      await payoutsInstance.submitPayout(
        currentPayoutData.competitionId, payableEntry.entry, invoice,
        currentPayoutData.payoutAmount, payableEntry.competition,
      );
    }

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

function toggleLightningAddressForm() {
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
 * Set up the payout dialog, and the payouts page's buttons. The page is
 * swapped in by htmx, so its buttons are handled by one listener here.
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

  document.addEventListener("click", (event) => {
    const button = event.target.closest?.("[data-payout-action]");
    if (!button) return;
    event.preventDefault();
    switch (button.dataset.payoutAction) {
      case "invoice":
        openPayoutModal(button);
        break;
      case "edit-address":
        toggleLightningAddressForm();
        break;
      case "save-address":
        saveLightningAddress();
        break;
    }
  });
}

window.Payouts = Payouts;
window.initPayouts = initPayouts;
window.setupPayoutModal = setupPayoutModal;
