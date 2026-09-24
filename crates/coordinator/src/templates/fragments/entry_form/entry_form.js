// Paying for and submitting an entry. The form itself is server-rendered;
// this is the part that needs the WASM wallet: entry keys, the Keymeld
// registration, the ticket payment and the signed submission.

class Entry {
  constructor(coordinator_url, oracle_url, competition) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new window.AuthorizedClient(
      session.nostrClient,
      coordinator_url,
    );
    this.competition = competition;
    this.ticket = null;
  }

  async init() {
    // The entry key is derived from the entry id, so every entry gets its own
    // key and no counter or entry ordering is involved.
    const id = generateUuidV7();
    const { ephemeral_pubkey, payout_hash } =
      session.dlcWallet.entryRegistration(id);

    this.entry = {
      id,
      competition_id: this.competition.id,
      submit: {},
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
      // The invoice's QR code, drawn by the server as an SVG image.
      payment_request_qr: ticketData.payment_request_qr,
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
      this.preparedRegistration = await session.dlcWallet.keymeldPayoutRegistration(
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
        ? await session.dlcWallet.keymeldRegistration(this.entry.id, JSON.stringify(assignment))
        : null;
    }
    // Ticket hash, wallet key and enclave trust are all checked before
    // exposing the invoice for payment. A failed check cannot leave a paid ticket.
    return this.showPaymentModal();
  }

  async showPaymentModal() {
    const $modal = document.getElementById("ticketPaymentModal");
    const $paymentRequest = document.getElementById("paymentRequest");
    const $copyFeedback = document.getElementById("copyFeedback");
    const $error = document.getElementById("ticketPaymentError");
    const $paymentStatus = document.getElementById("paymentStatus");
    const $qrContainer = document.getElementById("qrContainer");

    const updateStatus = (message, type = "info") => {
      const text = document.createElement("p");
      text.className = `has-text-${type}`;
      text.textContent = message;
      const progress = document.createElement("progress");
      progress.className = `progress is-${type}`;
      progress.max = 100;
      $paymentStatus.replaceChildren(text, progress);
    };

    // The server draws the QR code; an <img> of it can run nothing.
    const $qrCode = document.createElement("img");
    $qrCode.id = "paymentQR";
    $qrCode.className = "payment-qr";
    $qrCode.width = 300;
    $qrCode.height = 300;
    $qrCode.alt = "QR code of the Lightning invoice";
    if (this.ticket.payment_request_qr?.startsWith("data:image/svg+xml;")) {
      $qrCode.src = this.ticket.payment_request_qr;
    }

    const cleanup = () => {
      $qrContainer.replaceChildren();
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

    $qrContainer.replaceChildren($qrCode);
    $paymentRequest.value = this.ticket.payment_request;
    const $amount = document.getElementById("ticketPaymentAmount");
    if ($amount) {
      $amount.textContent = `Pay ${this.ticketAmountSats.toLocaleString("en-US")} sats by Lightning to enter this competition:`;
    }
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

      // Check the ticket every 2 s until it is paid or fails.
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

// Picks by station from the form's checked radios, named `KPWM_temp_high`.
function collectPicks(form) {
  const picks = {};
  for (const input of form.querySelectorAll('input[type="radio"]:checked')) {
    const separator = input.name.indexOf("_");
    const stationId = input.name.slice(0, separator);
    const metric = input.name.slice(separator + 1);
    picks[stationId] ??= {};
    picks[stationId][metric] = input.value;
  }
  return picks;
}

// Radios cannot be cleared by clicking them again; picks can.
function togglePick(input) {
  if (input.dataset.picked === "true") {
    input.checked = false;
  }
  for (const other of input.form?.querySelectorAll(`input[name="${input.name}"]`) ?? []) {
    other.dataset.picked = String(other.checked);
  }
}

function showLogin() {
  window.openModal?.(document.getElementById("loginModal"));
}

// The competition, its payout terms and the oracle's announcement, fetched
// fresh for the wallet to check against what the form showed.
async function loadEntryTerms(form) {
  const base = document.body.dataset.apiBase || "";
  const oracleBase = document.body.dataset.oracleBase || "";
  const competitionId = form.dataset.competitionId;
  const [competitionResponse, quoteResponse] = await Promise.all([
    fetch(`${base}/api/v1/competitions/${competitionId}`),
    fetch(`${base}/api/v1/competitions/${competitionId}/payout-terms`),
  ]);
  if (!competitionResponse.ok || !quoteResponse.ok) {
    throw new Error("Payout terms are unavailable right now; try again in a moment");
  }
  const competition = await competitionResponse.json();
  const quote = await quoteResponse.json();
  const event = competition.event_submission;
  if (competition.id !== competitionId || !event ||
      event.entry_fee !== Number(form.dataset.entryFee) ||
      ticketPriceSats(event) !== Number(form.dataset.ticketPrice) ||
      event.total_competition_pool !== Number(form.dataset.totalPool) ||
      event.number_of_places_win !== Number(form.dataset.winnerCount)) {
    throw new Error("This competition's terms changed since the form opened; go back to the competitions list and open it again");
  }
  let oracle = null;
  if (quote.enabled) {
    const oracleResponse = await fetch(`${oracleBase}/oracle/events/${competitionId}`);
    if (!oracleResponse.ok) throw new Error("The oracle announcement is unavailable; no ticket payment has been requested");
    oracle = await oracleResponse.json();
    if (oracle.id !== competitionId || !oracle.event_announcement) throw new Error("The oracle returned a different event");
  }
  return { competition, quote, oracle };
}

// The Lightning Address on the player's account: where automatic payouts,
// and Arkade refunds, are sent. Throws rather than dropping automatic payouts.
async function loadPayoutAddress() {
  const base = document.body.dataset.apiBase || "";
  const client = new window.AuthorizedClient(session.nostrClient, base);
  let response;
  try {
    response = await client.post(`${base}/api/v1/users/login`);
  } catch (error) {
    response = error.response;
  }
  if (!response?.ok) {
    throw new Error("Your account's Lightning Address could not be loaded; try again in a moment");
  }
  const user = await response.json();
  return user.lightning_address || null;
}

/**
 * Submit entry - handles the full flow:
 * 1. Collect picks from form
 * 2. Check the terms and payout address
 * 3. Request ticket (triggers payment)
 * 4. Submit entry after payment
 */
async function submitEntry() {
  const form = document.getElementById("entryForm");
  const submitBtn = document.getElementById("submitEntry");
  const errorMsg = document.getElementById("errorMessage");
  const successMsg = document.getElementById("successMessage");

  errorMsg.classList.add("hidden");
  errorMsg.textContent = "";
  successMsg.classList.add("hidden");

  if (!isLoggedIn() || !session.nostrClient || !session.dlcWallet) {
    showLogin();
    return;
  }
  if (typeof session.nostrClient.isSignerReady === "function" && !session.nostrClient.isSignerReady()) {
    errorMsg.textContent = "Session expired. Please log in again.";
    errorMsg.classList.remove("hidden");
    showLogin();
    return;
  }

  submitBtn.disabled = true;
  submitBtn.classList.add("is-loading");

  try {
    const competitionId = form.dataset.competitionId;
    const picks = collectPicks(form);
    let choiceCount = 0;
    for (const stationPicks of Object.values(picks)) {
      choiceCount += Object.keys(stationPicks).length;
    }
    if (choiceCount === 0) {
      throw new Error("Make at least one pick");
    }
    const maxValues = parseInt(form.dataset.maxValues, 10) || 1;
    if (choiceCount > maxValues) {
      throw new Error(`You can make up to ${maxValues} picks; you made ${choiceCount}`);
    }

    const payoutTerms = await loadEntryTerms(form);
    // Automatic payouts go to the account's address; without one, or for a
    // legacy competition, the winner submits an invoice instead.
    const address = payoutTerms.quote.enabled ? await loadPayoutAddress() : null;
    if (address && (address.length > 320 || !/^[a-z0-9_+.-]+@[a-z0-9.-]+$/.test(address))) {
      throw new Error("The Lightning Address on your account is not valid; update it on the Payouts page");
    }
    // The price shown on the form; the wallet refuses an invoice for any other amount.
    const ticketAmountSats = Number(form.dataset.ticketPrice);
    if (!Number.isSafeInteger(ticketAmountSats) || ticketAmountSats <= 0) {
      throw new Error("The competition is missing its ticket price");
    }

    const body = document.body;
    const currentEntry = new Entry(body.dataset.apiBase || "", body.dataset.oracleBase || "", {
      id: competitionId,
    });
    await currentEntry.init();
    currentEntry.payoutTerms = payoutTerms;
    currentEntry.ticketAmountSats = ticketAmountSats;
    currentEntry.payoutChoice = {
      entry_id: currentEntry.entry.id,
      payout_hash: currentEntry.entry.payout_hash,
      lightning_address: address,
      allow_invoice_fallback: true,
      release_entry_key_after_payment: true,
    };
    currentEntry.entry.submit = picks;

    await currentEntry.submit(currentEntry.buildExpectedObservations(picks));

    successMsg.classList.remove("hidden");
    submitBtn.textContent = "Entered";
    submitBtn.classList.remove("is-loading");
    submitBtn.classList.add("is-success");
  } catch (error) {
    console.error("Entry submission failed:", error);

    // WASM rejects with plain strings, which have no message property.
    const detail = error instanceof Error ? error.message : typeof error === "string" ? error : "";
    let userMessage = detail || "Failed to submit entry";
    if (detail.includes("No signer initialized")) {
      userMessage = "Session expired. Please log in again.";
      showLogin();
    } else if (detail.includes("NetworkError")) {
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
  if (!element || !session.wasm?.DlcWallet) return;
  try {
    const trust = session.wasm.DlcWallet.keymeldTrust();
    if (trust.source === "pinned") {
      const pins = Object.entries(trust.pcrs)
        .map(([index, value]) => `PCR${index} ${value}`)
        .join(", ");
      element.textContent = `Your entry key is sent only to a Keymeld enclave attested against ${pins}.`;
    } else {
      element.textContent =
        "Test network: this build pins no Keymeld enclave measurements, so enclave trust comes from the coordinator.";
    }
  } catch (error) {
    element.textContent = `Keymeld enclave trust unavailable: ${error}`;
  }
  element.classList.remove("is-hidden");
}

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

// What the server charges for a ticket: the entry fee plus the coordinator fee,
// rounded the same way as Competition::calculate_invoice_amount.
function ticketPriceSats(event) {
  return event.entry_fee + Math.round(event.entry_fee * (event.coordinator_fee_percentage / 100));
}

function setupEntryForm() {
  document.addEventListener("click", (event) => {
    if (event.target.closest?.("#submitEntry")) {
      submitEntry();
      return;
    }
    const pick = event.target.closest?.('.pick-option input[type="radio"]');
    if (pick) togglePick(pick);
  });
}

window.showKeymeldTrust = showKeymeldTrust;
window.submitEntry = submitEntry;
window.collectPicks = collectPicks;
window.loadEntryTerms = loadEntryTerms;
window.ticketPriceSats = ticketPriceSats;
window.setupEntryForm = setupEntryForm;
