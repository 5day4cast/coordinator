class Entry {
  constructor(coordinator_url, oracle_url, competition) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new window.AuthorizedClient(
      window.nostrClient,
      coordinator_url,
    );
    this.competition = competition;
    this.ticket = null;
  }

  async init() {
    const [competition_forecasts, _] = await Promise.all([
      this.getCompetitionLastForecast(),
      this.setupEntry(),
    ]);

    this.competition_forecasts = competition_forecasts;

    for (const station_id in competition_forecasts) {
      const forecast = competition_forecasts[station_id];
      this.entry.options.push({
        station_id,
        date: forecast.date,
        temp_high: forecast.temp_high,
        temp_low: forecast.temp_low,
        wind_speed: forecast.wind_speed,
      });
      this.entry.submit[station_id] = {};
    }
  }

  async getCompetitionLastForecast() {
    // Forecasts are already rendered server-side in the form, extract them from there
    // This avoids needing a separate API call and keeps the data consistent
    return this.getForecastsFromForm();
  }

  // Extract forecast data from the rendered entry form (for mock/test mode)
  getForecastsFromForm() {
    const forecasts = {};
    const stationBoxes = document.querySelectorAll("#entryForm [data-station]");

    stationBoxes.forEach((box) => {
      const stationId = box.dataset.station;
      forecasts[stationId] = {
        date: new Date().toISOString().split("T")[0],
        temp_high: null,
        temp_low: null,
        wind_speed: null,
      };

      // Parse values from the form labels if present
      box.querySelectorAll(".field").forEach((field) => {
        const label = field.querySelector(".label")?.textContent || "";
        if (label.includes("Wind Speed")) {
          forecasts[stationId].wind_speed = this.parseValueFromLabel(label);
        } else if (label.includes("High Temp")) {
          forecasts[stationId].temp_high = this.parseValueFromLabel(label);
        } else if (label.includes("Low Temp")) {
          forecasts[stationId].temp_low = this.parseValueFromLabel(label);
        }
      });
    });

    return forecasts;
  }

  parseValueFromLabel(label) {
    // Extract numeric value from labels like "Wind Speed: 12.5 mph"
    const match = label.match(/:\s*([\d.]+)/);
    return match ? parseFloat(match[1]) : null;
  }

  async setupEntry() {
    // The entry key is derived from the entry id, so every entry gets its own
    // key and no counter or entry ordering is involved.
    const id = generateUuidV7();
    const { ephemeral_pubkey, payout_hash } =
      window.dlcWallet.entryRegistration(id);

    this.entry = {
      id,
      competition_id: this.competition.id,
      submit: {},
      options: [],
      payout_hash,
      ephemeral_pubkey,
    };
  }

  async handleTicketPayment(btc_pubkey) {
    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/ticket`,
      { btc_pubkey, payout: this.payoutChoice },
    );

    if (!response.ok)
      throw new Error(`Failed to get ticket: ${response.status}`);

    const ticketData = await response.json();
    this.ticket = {
      id: ticketData.ticket_id,
      payment_request: ticketData.payment_request,
      keymeld_session_id: ticketData.keymeld_session_id,
      keymeld_enclave_public_key: ticketData.keymeld_enclave_public_key,
      keymeld_user_id: ticketData.keymeld_user_id,
      keymeld_registration: ticketData.keymeld_registration,
    };

    const assignment = this.ticket.keymeld_registration;
    if (this.payoutTerms?.quote.enabled && !assignment?.payout_policy) {
      throw new Error("The ticket omitted the approved payout escrow policy");
    }
    if (this.ticket.keymeld_session_id && !assignment) {
      throw new Error("The ticket is missing its authorized Keymeld registration context");
    }
    if (assignment && (assignment.user_id !== this.ticket.id ||
        assignment.session_id !== this.ticket.keymeld_session_id)) {
      throw new Error("The Keymeld registration belongs to another ticket or session");
    }
    if (assignment?.payout_policy) {
      this.preparedRegistration = await window.dlcWallet.keymeldPayoutRegistration(
        this.entry.id,
        JSON.stringify(assignment),
        JSON.stringify({
          competition_id: this.competition.id,
          lightning_address: this.payoutChoice.lightning_address,
          allow_invoice_fallback: this.payoutChoice.allow_invoice_fallback,
          release_entry_key_after_payment: this.payoutChoice.release_entry_key_after_payment,
          ticket_invoice: this.ticket.payment_request,
          ticket_amount_sats: this.ticketAmountSats,
          expected_funding_sats: this.payoutTerms.competition.event_submission.total_competition_pool,
          expected_player_count: this.payoutTerms.competition.event_submission.total_allowed_entries,
          expected_winner_count: this.payoutTerms.competition.event_submission.number_of_places_win,
          expected_relative_locktime_delta: this.payoutTerms.quote.relative_locktime_block_delta,
          max_fee_rate_sat_vb: this.payoutTerms.quote.max_fee_rate_sat_vb,
          oracle_announcement: this.payoutTerms.oracle.event_announcement,
        }),
      );
    } else {
      if (this.payoutChoice.lightning_address) {
        throw new Error("Automatic payouts are unavailable for this competition. Choose invoice payout or another competition.");
      }
      this.preparedRegistration = assignment
        ? await window.dlcWallet.keymeldRegistration(this.entry.id, JSON.stringify(assignment))
        : null;
    }
    // Consent, ticket hash, wallet key and enclave trust are all checked before
    // exposing the invoice for payment. A failed check cannot leave a paid ticket.
    return this.showPaymentModal();
  }

  showPaymentModal() {
    const $modal = document.getElementById("ticketPaymentModal");
    const $paymentRequest = document.getElementById("paymentRequest");
    const $copyFeedback = document.getElementById("copyFeedback");
    const $error = document.getElementById("ticketPaymentError");
    const $paymentStatus = document.getElementById("paymentStatus");
    const $qrContainer = document.getElementById("qrContainer");

    const updateStatus = (message, type = "info") => {
      $paymentStatus.innerHTML = `
                <p class="has-text-${type}">${message}</p>
                <progress class="progress is-${type}" max="100"></progress>
            `;
    };

    const $qrCode = document.createElement("bitcoin-qr");
    Object.assign($qrCode, {
      id: "paymentQR",
      lightning: this.ticket.payment_request,
      width: 300,
      height: 300,
      type: "svg",
      isPolling: true,
      pollInterval: 2000,
    });

    [
      "dots-type:rounded",
      "corners-square-type:extra-rounded",
      "background-color:#ffffff",
      "dots-color:#000000",
    ].forEach((attr) => {
      const [key, value] = attr.split(":");
      $qrCode.setAttribute(key, value);
    });

    const cleanup = () => {
      $qrCode.setAttribute("is-polling", "false");
      $qrContainer.innerHTML = "";
      $modal.classList.remove("is-active");
      $copyFeedback.classList.add("is-hidden");
      $paymentRequest.classList.remove("is-success");
    };

    const handleCopy = async () => {
      try {
        await navigator.clipboard.writeText($paymentRequest.value);
        $paymentRequest.classList.add("is-success");
        $copyFeedback.classList.remove("is-hidden");
        setTimeout(() => {
          $copyFeedback.classList.add("is-hidden");
          $paymentRequest.classList.remove("is-success");
        }, 2000);
      } catch (err) {
        console.error("Failed to copy:", err);
      }
    };

    $paymentRequest.addEventListener("click", handleCopy);

    $qrContainer.innerHTML = "";
    $qrContainer.appendChild($qrCode);
    $paymentRequest.value = this.ticket.payment_request;
    updateStatus("Waiting for payment...");
    $error.classList.add("is-hidden");
    $modal.classList.add("is-active");

    return new Promise((resolve, reject) => {
      let currentStatus = "Reserved";
      let closeHandlersRemoved = false;
      let backgroundPollingInterval = null;
      let fallbackPollingInterval = null;
      let resolved = false;

      const checkPaymentStatus = async () => {
        if (resolved) return;
        try {
          const response = await this.client.get(
            `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/tickets/${this.ticket.id}/status`,
          );

          if (!response.ok)
            throw new Error(
              `Failed to check ticket status: ${response.status}`,
            );

          const status = await response.json();
          currentStatus = status;

          if (status === "Settled" || status === "Paid") {
            resolved = true;
            removeCloseHandlers();
            if (backgroundPollingInterval) {
              clearInterval(backgroundPollingInterval);
            }
            if (fallbackPollingInterval) {
              clearInterval(fallbackPollingInterval);
            }
            updateStatus("Payment received!", "success");
            cleanup();
            resolve(true);
            return true;
          } else if (status === "Reserved") {
            return false;
          }

          const errorMessages = {
            Expired: "Ticket payment expired. Please request a new ticket.",
            Used: "Ticket has already been used.",
            Cancelled: "Competition has been cancelled.",
          };

          throw new Error(
            errorMessages[status] || `Unexpected ticket status: ${status}`,
          );
        } catch (error) {
          if (!resolved) {
            resolved = true;
            if (fallbackPollingInterval) {
              clearInterval(fallbackPollingInterval);
            }
            $error.textContent = error.message;
            $error.classList.remove("is-hidden");
            updateStatus("Payment failed", "danger");
            cleanup();
            reject(error);
          }
          return false;
        }
      };

      // Fallback polling in case the QR component's callback doesn't fire
      fallbackPollingInterval = setInterval(checkPaymentStatus, 2000);

      // Background polling function to check payment status after modal is closed
      const startBackgroundPolling = () => {
        backgroundPollingInterval = setInterval(async () => {
          if (resolved) {
            clearInterval(backgroundPollingInterval);
            return;
          }
          try {
            const response = await this.client.get(
              `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/tickets/${this.ticket.id}/status`,
            );
            if (response.ok) {
              const status = await response.json();
              if (status === "Settled" || status === "Paid") {
                resolved = true;
                clearInterval(backgroundPollingInterval);
                resolve(true);
              }
            }
          } catch (e) {
            // Ignore errors during background polling
          }
        }, 2000);
      };

      const handleClose = () => {
        // Only allow cancellation if payment hasn't been received yet
        if (currentStatus === "Paid" || currentStatus === "Settled") {
          // Payment already received, don't cancel - just close modal and resolve
          cleanup();
          if (!resolved) {
            resolved = true;
            resolve(true);
          }
          return;
        }
        // Modal closed but payment might still be in-flight
        // Start background polling to detect if payment completes
        if (fallbackPollingInterval) {
          clearInterval(fallbackPollingInterval);
        }
        cleanup();
        startBackgroundPolling();
        // Don't reject yet - let background polling resolve if payment comes through
        // Set a timeout to eventually reject if no payment after 5 minutes
        setTimeout(
          () => {
            if (backgroundPollingInterval && !resolved) {
              clearInterval(backgroundPollingInterval);
              reject(new Error("Payment cancelled by user"));
            }
          },
          5 * 60 * 1000,
        );
      };

      const $modalClose = $modal.querySelector(".modal-close");

      // Only allow closing via the X button, not by clicking the background
      // This prevents accidental closure while payment is in-flight
      $modalClose.addEventListener("click", handleClose, { once: true });

      const removeCloseHandlers = () => {
        if (closeHandlersRemoved) return;
        closeHandlersRemoved = true;
        $modalClose.removeEventListener("click", handleClose);
      };

      // Also set QR code callback for when the component supports it
      $qrCode.callback = checkPaymentStatus;
    });
  }

  async submit(expectedObservations) {
    try {
      await this.handleTicketPayment(this.entry.ephemeral_pubkey);

      const keymeldData = this.preparedRegistration;
      const encrypted_keymeld_private_key = keymeldData?.encrypted_private_key ?? null;
      const keymeld_auth_pubkey = keymeldData?.auth_pubkey ?? null;
      const keymeld_registration_context = keymeldData?.context ?? null;
      const keymeld_escrow_policy = keymeldData?.escrow_policy ?? null;

      const entry_body = {
        id: this.entry.id,
        ephemeral_pubkey: this.entry.ephemeral_pubkey,
        payout_hash: this.entry.payout_hash,
        event_id: this.competition.id,
        ticket_id: this.ticket.id,
        expected_observations: expectedObservations,
        encrypted_keymeld_private_key,
        keymeld_auth_pubkey,
        keymeld_registration_context,
        keymeld_escrow_policy,
      };

      const response = await this.client.post(
        `${this.coordinator_url}/api/v1/entries`,
        entry_body,
      );

      if (!response.ok)
        throw new Error(`Failed to create entry, status: ${response.status}`);

      return await response.json();
    } catch (e) {
      console.error("Error submitting entry:", e);
      throw e;
    }
  }

  buildExpectedObservations(submit) {
    return Object.entries(submit).map(([station_id, choices]) => ({
      stations: station_id,
      ...Object.entries(choices).reduce((acc, [weather_type, selected_val]) => {
        acc[weather_type] = this.convertSelectVal(selected_val);
        return acc;
      }, {}),
    }));
  }

  convertSelectVal(raw_select) {
    const valueMap = { par: "Par", over: "Over", under: "Under" };
    if (!(raw_select in valueMap))
      throw new Error(`Invalid selection: ${raw_select}`);
    return valueMap[raw_select];
  }
}

window.Entry = Entry;

// Current entry instance for the form
let currentEntry = null;

/**
 * Handle pick button selection (Over/Par/Under)
 * Called when user clicks a prediction button
 * Clicking an already-selected button will deselect it (toggle behavior)
 */
function selectPick(button) {
  const field = button.dataset.field;
  const value = button.dataset.value;
  const wasActive = button.classList.contains("is-active");

  // Find all buttons in this group and deselect them
  const group = button.closest(".buttons");
  group.querySelectorAll(".pick-button").forEach((btn) => {
    btn.classList.remove("is-active");
    btn.classList.add("is-outlined");
  });

  // Update hidden input and entry state
  const hiddenInput = document.getElementById(field);

  if (wasActive) {
    // Button was already selected - deselect it (toggle off)
    if (hiddenInput) {
      hiddenInput.value = "";
    }

    // Remove from current entry if exists
    if (currentEntry) {
      const parts = field.split("_");
      const stationId = parts[0];
      const metric = parts.slice(1).join("_");

      if (currentEntry.entry.submit[stationId]) {
        delete currentEntry.entry.submit[stationId][metric];
        // Clean up empty station objects
        if (Object.keys(currentEntry.entry.submit[stationId]).length === 0) {
          delete currentEntry.entry.submit[stationId];
        }
      }
    }
  } else {
    // Select clicked button - color is determined by CSS based on data-value
    button.classList.remove("is-outlined");
    button.classList.add("is-active");

    if (hiddenInput) {
      hiddenInput.value = value;
    }

    // Update current entry if exists
    if (currentEntry) {
      // Parse field name: stationId_metric (e.g., "KEWR_wind_speed")
      const parts = field.split("_");
      const stationId = parts[0];
      const metric = parts.slice(1).join("_");

      if (!currentEntry.entry.submit[stationId]) {
        currentEntry.entry.submit[stationId] = {};
      }
      currentEntry.entry.submit[stationId][metric] = value;
    }
  }
}

/**
 * Submit entry - handles the full flow:
 * 1. Collect picks from form
 * 2. Create Entry instance
 * 3. Request ticket (triggers payment)
 * 4. Submit entry after payment
 */
async function submitEntry() {
  const form = document.getElementById("entryForm");
  const submitBtn = document.getElementById("submitEntry");
  const errorMsg = document.getElementById("errorMessage");
  const successMsg = document.getElementById("successMessage");

  // Reset messages
  errorMsg.classList.add("hidden");
  errorMsg.textContent = "";
  successMsg.classList.add("hidden");

  // Check if user is logged in
  if (typeof window.isLoggedIn === "function" && !window.isLoggedIn()) {
    // Show login modal
    const loginModal = document.getElementById("loginModal");
    if (loginModal) {
      loginModal.classList.add("is-active");
    }
    return;
  }

  // Double-check that required WASM objects are ready
  if (!window.nostrClient || !window.dlcWallet) {
    errorMsg.textContent = "Please log in to submit an entry";
    errorMsg.classList.remove("hidden");
    const loginModal = document.getElementById("loginModal");
    if (loginModal) {
      loginModal.classList.add("is-active");
    }
    return;
  }

  // Verify the signer is actually ready
  if (
    typeof window.nostrClient.isSignerReady === "function" &&
    !window.nostrClient.isSignerReady()
  ) {
    errorMsg.textContent = "Session expired. Please log in again.";
    errorMsg.classList.remove("hidden");
    const loginModal = document.getElementById("loginModal");
    if (loginModal) {
      loginModal.classList.add("is-active");
    }
    return;
  }

  // Disable button during submission
  submitBtn.disabled = true;
  submitBtn.classList.add("is-loading");

  try {
    const competitionId = form.dataset.competitionId;

    // Collect all picks from hidden inputs
    const picks = {};
    form.querySelectorAll('input[type="hidden"]').forEach((input) => {
      if (input.value) {
        const parts = input.name.split("_");
        const stationId = parts[0];
        const metric = parts.slice(1).join("_");

        if (!picks[stationId]) {
          picks[stationId] = {};
        }
        picks[stationId][metric] = input.value;
      }
    });

    // Count total value choices
    let choiceCount = 0;
    for (const stationPicks of Object.values(picks)) {
      choiceCount += Object.keys(stationPicks).length;
    }

    // Validate we have picks
    if (choiceCount === 0) {
      throw new Error("Please make at least one prediction");
    }

    // Validate we don't have too many picks
    const maxValues = parseInt(form.dataset.maxValues, 10) || 1;
    if (choiceCount > maxValues) {
      throw new Error(
        `Too many predictions selected. Maximum allowed: ${maxValues}, but you selected: ${choiceCount}`,
      );
    }

    // Get API config from body data attributes
    const body = document.body;
    const apiBase = body.dataset.apiBase || "";
    const oracleBase = body.dataset.oracleBase || "";

    const competition = {
      id: competitionId,
    };

    // Create entry instance
    currentEntry = new Entry(apiBase, oracleBase, competition);
    await currentEntry.init();

    const payoutTerms = window.entryPayoutTerms;
    if (!payoutTerms || payoutTerms.competition.id !== competitionId) {
      throw new Error("Payout terms are not ready; reload the entry form and try again");
    }
    currentEntry.payoutTerms = payoutTerms;
    const approved = document.getElementById("entryPayoutApproved")?.checked;
    if (!approved) throw new Error("Please approve your payout method before paying for this entry");
    const automatic = document.getElementById("entryPayoutMethod")?.value === "automatic";
    const address = automatic
      ? (document.getElementById("entryLightningAddress")?.value || "").trim().toLowerCase()
      : null;
    if (automatic && (!address || address.length > 320 || !/^[a-z0-9_+.-]+@[a-z0-9.-]+$/.test(address))) {
      throw new Error("Enter a valid Lightning Address for automatic payouts");
    }
    currentEntry.ticketAmountSats = Number(form.dataset.entryFee);
    if (!Number.isSafeInteger(currentEntry.ticketAmountSats) || currentEntry.ticketAmountSats <= 0) {
      throw new Error("The competition is missing its entry fee");
    }
    currentEntry.payoutChoice = {
      entry_id: currentEntry.entry.id,
      payout_hash: currentEntry.entry.payout_hash,
      lightning_address: address,
      allow_invoice_fallback: true,
      release_entry_key_after_payment: true,
    };
    // Set picks on entry
    currentEntry.entry.submit = picks;

    // Build expected observations for submission
    const expectedObservations = currentEntry.buildExpectedObservations(picks);

    // Submit entry (handles ticket payment internally)
    await currentEntry.submit(expectedObservations);

    // Success!
    successMsg.classList.remove("hidden");
    submitBtn.textContent = "Entry Submitted!";
    submitBtn.classList.remove("is-loading");
    submitBtn.classList.add("is-success");
  } catch (error) {
    console.error("Entry submission failed:", error);

    // Provide user-friendly error messages
    let userMessage = error.message || "Failed to submit entry";
    if (error.message && error.message.includes("No signer initialized")) {
      userMessage = "Session expired. Please log in again.";
      const loginModal = document.getElementById("loginModal");
      if (loginModal) {
        loginModal.classList.add("is-active");
      }
    } else if (error.message && error.message.includes("NetworkError")) {
      userMessage =
        "Network error. Please check your connection and try again.";
    }

    errorMsg.textContent = userMessage;
    errorMsg.classList.remove("hidden");
    submitBtn.disabled = false;
    submitBtn.classList.remove("is-loading");
  }
}

/**
 * Show where this build's keymeld enclave trust comes from. The measurements
 * are compiled into the WASM, so they can be checked against Keymeld's
 * published release measurements.
 */
function showKeymeldTrust() {
  const element = document.getElementById("keymeldTrust");
  if (!element || !window.DlcWallet) return;
  try {
    const trust = window.DlcWallet.keymeldTrust();
    if (trust.source === "pinned") {
      const pins = Object.entries(trust.pcrs)
        .map(([index, value]) => `PCR${index} ${value}`)
        .join(", ");
      element.textContent = `Your entry key is sent only to a Keymeld enclave attested against ${pins}.`;
    } else {
      element.textContent =
        "Test network: this build pins no Keymeld enclave measurements, so enclave trust comes from the coordinator.";
      element.classList.add("has-text-warning");
    }
  } catch (error) {
    element.textContent = `Keymeld enclave trust unavailable: ${error}`;
    element.classList.add("has-text-danger");
  }
}

window.showKeymeldTrust = showKeymeldTrust;
window.selectPick = selectPick;
window.submitEntry = submitEntry;

// Generate UUIDv7 (time-ordered UUID)
function generateUuidV7() {
  const timestamp = Date.now();
  const timestampHex = timestamp.toString(16).padStart(12, "0");

  // Get random bytes for the rest
  const randomBytes = new Uint8Array(10);
  crypto.getRandomValues(randomBytes);

  // Build UUIDv7: tttttttt-tttt-7xxx-yxxx-xxxxxxxxxxxx
  // t = timestamp, 7 = version, y = variant (8, 9, a, or b), x = random
  const hex = Array.from(randomBytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

  return [
    timestampHex.slice(0, 8),
    timestampHex.slice(8, 12),
    "7" + hex.slice(0, 3),
    ((parseInt(hex.slice(3, 4), 16) & 0x3) | 0x8).toString(16) +
      hex.slice(4, 7),
    hex.slice(7, 19),
  ].join("-");
}


function updateEntryPayoutMethod() {
  const automatic = document.getElementById("entryPayoutMethod")?.value === "automatic";
  document.getElementById("entryPayoutAddressField")?.classList.toggle("is-hidden", !automatic);
  const consent = document.getElementById("entryPayoutApproved");
  if (consent) consent.checked = false;
}

async function setupEntryPayoutConsent() {
  const input = document.getElementById("entryLightningAddress");
  const form = document.getElementById("entryForm");
  if (!input || !form) return;
  window.entryPayoutTerms = null;
  const resetConsent = () => {
    const consent = document.getElementById("entryPayoutApproved");
    if (consent) consent.checked = false;
  };
  input.addEventListener("input", resetConsent);
  const base = document.body.dataset.apiBase || "";
  const oracleBase = document.body.dataset.oracleBase || "";
  const competitionId = form.dataset.competitionId;
  const termsText = document.getElementById("entryPayoutTermsText");
  try {
    const [competitionResponse, quoteResponse] = await Promise.all([
      fetch(`${base}/api/v1/competitions/${competitionId}`),
      fetch(`${base}/api/v1/competitions/${competitionId}/payout-terms`),
    ]);
    if (!competitionResponse.ok || !quoteResponse.ok) throw new Error("Payout terms are unavailable");
    const competition = await competitionResponse.json();
    const quote = await quoteResponse.json();
    const event = competition.event_submission;
    if (competition.id !== competitionId || !event ||
        event.entry_fee !== Number(form.dataset.entryFee) ||
        event.total_competition_pool !== Number(form.dataset.totalPool) ||
        event.number_of_places_win !== Number(form.dataset.winnerCount)) {
      throw new Error("The displayed competition terms changed; reload this page");
    }
    let oracle = null;
    if (quote.enabled) {
      const oracleResponse = await fetch(`${oracleBase}/oracle/events/${competitionId}`);
      if (!oracleResponse.ok) throw new Error("The oracle announcement is unavailable; no ticket payment has been requested");
      oracle = await oracleResponse.json();
      if (oracle.id !== competitionId || !oracle.event_announcement) throw new Error("The oracle returned a different event");
      const percentages = {1:[100],2:[60,40],3:[45,35,20],4:[42,30,18,10],5:[40,27,16,9,8]}[event.number_of_places_win];
      if (!percentages) throw new Error("Unsupported winner distribution");
      if (termsText) termsText.textContent = `Pool: ${event.total_competition_pool} sats across ${event.total_allowed_entries} entries. Winner shares by rank: ${percentages.join("%, ")}%. Contract delay: ${quote.relative_locktime_block_delta} blocks. Maximum Bitcoin fee rate: ${quote.max_fee_rate_sat_vb} sat/vB.`;
    } else {
      const method = document.getElementById("entryPayoutMethod");
      method.value = "invoice";
      const automatic = method.querySelector('option[value="automatic"]');
      if (automatic) automatic.disabled = true;
      updateEntryPayoutMethod();
      if (termsText) termsText.textContent = "This legacy competition does not use payout escrow. Invoice recovery releases entry secrets before payment, which is not guaranteed.";
      const consentText = document.getElementById("entryPayoutConsentText");
      if (consentText) consentText.textContent = " I understand this legacy entry uses recovery that releases its claim before payment.";
    }
    resetConsent();
    window.entryPayoutTerms = { competition, quote, oracle };
  } catch (error) {
    if (termsText) termsText.textContent = error.message;
    return;
  }
  if (!window.nostrClient) return;
  try {
    const client = new window.AuthorizedClient(window.nostrClient, base);
    const response = await client.post(`${base}/api/v1/users/login`);
    const user = await response.json();
    if (!input.value && user.lightning_address) {
      input.value = user.lightning_address;
      resetConsent();
    }
  } catch (_) {
    // Address entry remains available when profile lookup fails.
  }
}
window.updateEntryPayoutMethod = updateEntryPayoutMethod;
window.setupEntryPayoutConsent = setupEntryPayoutConsent;
