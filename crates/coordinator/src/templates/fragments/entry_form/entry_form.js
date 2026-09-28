// Paying for and submitting an entry. The form itself is server-rendered;
// this is the part that needs the WASM wallet: entry keys, the Keymeld
// registration, the ticket payment and the signed submission.

class Entry {
  constructor(coordinator_url, oracle_url, competition) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new AuthorizedClient(
      session.nostrClient,
      coordinator_url,
    );
    this.competition = competition;
    this.ticket = null;
  }

  async init() {
    // The entry key is derived from the entry id, so every entry gets its own
    // key and no counter or entry ordering is involved.
    const id = session.wasm.DlcWallet.newEntryId();
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
    // What the wallet checks the invoice and the escrow against: the price the form showed,
    // plus the network fee fixed on this ticket.
    this.ticketAmountSats = ticketTotalSats(ticketData, this.shownPrice);
    showNetworkFee(ticketData.network_fee_sats, this.ticketAmountSats);
    if (assignment?.payout_policy && this.payoutTerms.queued) {
      // A queued entry names no pool yet: the wallet checks its terms against the
      // oracle's reference event and key and what the form showed.
      this.preparedRegistration = await session.dlcWallet.keymeldQueuedRegistration(
        this.entry.id,
        JSON.stringify(assignment),
        JSON.stringify({
          competition_id: this.competition.id,
          lightning_address: this.payoutChoice.lightning_address,
          allow_invoice_fallback: this.payoutChoice.allow_invoice_fallback,
          release_entry_key_after_payment: this.payoutChoice.release_entry_key_after_payment,
          ticket_invoice: this.ticket.payment_request,
          ticket_amount_sats: this.ticketAmountSats,
          expected_relative_locktime_delta: this.payoutTerms.quote.relative_locktime_block_delta,
          max_fee_rate_sat_vb: this.payoutTerms.quote.max_fee_rate_sat_vb,
          ...this.payoutTerms.queued,
        }),
      );
    } else if (assignment?.payout_policy) {
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
    // The registration is sent before the invoice is shown, so a ticket paid
    // for but never entered can still be refunded.
    if (this.preparedRegistration) await this.sendRegistration();
    return this.showPaymentModal();
  }

  async sendRegistration() {
    const registration = this.preparedRegistration;
    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/tickets/${this.ticket.id}/registration`,
      {
        ephemeral_pubkey: this.entry.ephemeral_pubkey,
        encrypted_keymeld_private_key: registration.encrypted_private_key,
        keymeld_auth_pubkey: registration.auth_pubkey,
        keymeld_registration_context: registration.context,
        keymeld_escrow_policy: registration.escrow_policy ?? null,
      },
    );
    if (!response.ok)
      throw new Error(`Failed to register the ticket: ${response.status}`);
  }

  // Shows the invoice and waits for it to be paid. The ticket's status is an
  // htmx fragment, signed like the account pages, that polls every 2 s until
  // it is paid or fails (see ticket_status in mod.rs) and then says so with an
  // fw:ticket-paid or fw:ticket-failed event. Closing the dialog keeps it
  // polling, hidden, for up to 5 minutes, since a payment may be in flight.
  async showPaymentModal() {
    const $modal = document.getElementById("ticketPaymentModal");
    const $paymentRequest = document.getElementById("paymentRequest");
    const $copyFeedback = document.getElementById("copyFeedback");
    const $error = document.getElementById("ticketPaymentError");
    const $qrContainer = document.getElementById("qrContainer");

    // The wallet draws the QR code from the invoice after checking that it
    // charges the ticket price on this network; an <img> of it can run nothing.
    const invoice = this.ticket.payment_request.trim();
    const $qrCode = document.createElement("img");
    $qrCode.id = "paymentQR";
    $qrCode.className = "payment-qr";
    $qrCode.width = 300;
    $qrCode.height = 300;
    $qrCode.alt = "QR code of the Lightning invoice";
    $qrCode.src = session.dlcWallet.invoiceQr(invoice, this.ticketAmountSats);
    $qrCode.draggable = false;
    // Tapping the code copies the invoice itself, not the image (which on
    // iOS would hand over the SVG data URL).
    const $qrButton = document.createElement("button");
    $qrButton.type = "button";
    $qrButton.className = "payment-qr-button";
    $qrButton.setAttribute("aria-label", "Copy the Lightning invoice");
    $qrButton.append($qrCode);
    $qrContainer.replaceChildren($qrButton);

    // Deep links into wallet apps, set only after the invoice was checked above.
    document.getElementById("walletLinkLightning").href = `lightning:${invoice}`;
    document.getElementById("walletLinkZeus").href = `zeusln:lightning:${invoice}`;
    // Cash App pays only mainnet invoices (lnbc…, but lnbcrt… is regtest):
    // https://docs.voltageapi.com/wallet-deep-linking
    const $cashApp = document.getElementById("walletLinkCashApp");
    const mainnet = /^lnbc(?!rt)/i.test(invoice);
    $cashApp.classList.toggle("is-hidden", !mainnet);
    if (mainnet) $cashApp.href = `https://cash.app/launch/lightning/${invoice}`;

    $paymentRequest.value = invoice;
    const copyInvoice = async () => {
      try {
        await navigator.clipboard.writeText(invoice);
      } catch (err) {
        // Older iOS Safari: fall back to selecting and copying the textarea.
        $paymentRequest.select();
        if (!document.execCommand("copy")) {
          console.error("Failed to copy:", err);
          return;
        }
      }
      $copyFeedback.classList.remove("is-hidden");
      setTimeout(() => $copyFeedback.classList.add("is-hidden"), 2000);
    };
    $paymentRequest.onclick = copyInvoice;
    $qrButton.onclick = copyInvoice;
    const networkFee = this.ticketAmountSats - this.shownPrice.ticketPrice;
    document.getElementById("ticketPaymentAmount").textContent =
      `Pay ${formatSats(this.ticketAmountSats)} by Lightning to enter this competition` +
      (networkFee > 0 ? `, including its ${formatSats(networkFee)} network fee:` : ":");

    const status = document.createElement("div");
    status.id = "paymentStatus";
    status.className = "mt-4";
    status.setAttribute("hx-get", `/competitions/${this.competition.id}/tickets/${this.ticket.id}/status`);
    status.setAttribute("hx-trigger", "every 2s");
    status.setAttribute("hx-swap", "outerHTML");
    status.textContent = "Waiting for payment...";
    document.getElementById("paymentStatus").replaceWith(status);
    window.htmx.process(status);
    $error.classList.add("is-hidden");
    $modal.classList.add("is-active");

    return new Promise((resolve, reject) => {
      const finish = (error) => {
        document.removeEventListener("fw:ticket-paid", paid);
        document.removeEventListener("fw:ticket-failed", failed);
        clearTimeout(giveUp);
        // A poller that leaves the page stops; close the dialog.
        const idle = document.createElement("div");
        idle.id = "paymentStatus";
        document.getElementById("paymentStatus")?.replaceWith(idle);
        $qrContainer.replaceChildren();
        document.querySelectorAll("#walletLinks a").forEach((a) => a.removeAttribute("href"));
        $modal.classList.remove("is-active");
        if (error) {
          $error.textContent = error.message;
          $error.classList.remove("is-hidden");
          reject(error);
        } else {
          resolve(true);
        }
      };
      const paid = () => finish();
      const failed = (event) => finish(new Error(event.detail?.message || "The ticket payment failed"));
      const giveUp = setTimeout(() => finish(new Error("Payment cancelled by user")), 5 * 60 * 1000);
      document.addEventListener("fw:ticket-paid", paid);
      document.addEventListener("fw:ticket-failed", failed);
      // Closing hides the dialog; polling goes on until paid or given up.
      $modal.querySelector(".modal-close").onclick = () => $modal.classList.remove("is-active");
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


// Picks by station from the form's checked radios, named `KPWM_temp_high`.
// "No pick" has an empty value.
function collectPicks(form) {
  const picks = {};
  for (const input of form.querySelectorAll('input[type="radio"]:checked')) {
    if (!input.value) continue;
    const separator = input.name.indexOf("_");
    const stationId = input.name.slice(0, separator);
    const metric = input.name.slice(separator + 1);
    picks[stationId] ??= {};
    picks[stationId][metric] = input.value;
  }
  return picks;
}

function showLogin() {
  openModal(document.getElementById("loginModal"));
}

const TERMS_CHANGED = "This competition's terms changed since the form opened; go back to the competitions list and open it again";

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
  // A competition that doesn't say is a single one, and so is its form.
  const kind = competition.kind ?? "single";
  if (competition.id !== competitionId || !event ||
      kind !== (form.dataset.kind ?? "single") ||
      event.entry_fee !== Number(form.dataset.entryFee) ||
      ticketPriceSats(event) !== Number(form.dataset.ticketPrice) ||
      event.total_competition_pool !== Number(form.dataset.totalPool) ||
      event.number_of_places_win !== Number(form.dataset.winnerCount)) {
    throw new Error(TERMS_CHANGED);
  }
  if (kind === "queued") {
    return { competition, quote, oracle: null, queued: await loadQueueTerms(form, competition, quote) };
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

// What a queued entry's terms are checked against: the entry fee and pool
// sizes the form shows, and the competition's reference event and the
// oracle's key, both straight from the oracle.
async function loadQueueTerms(form, competition, quote) {
  const oracleBase = document.body.dataset.oracleBase || "";
  const shown = {
    min_players: Number(form.dataset.poolMinPlayers),
    max_players: Number(form.dataset.poolMaxPlayers),
  };
  if (!Number.isSafeInteger(shown.min_players) || !Number.isSafeInteger(shown.max_players) ||
      competition.pool_rules?.min_players !== shown.min_players ||
      competition.pool_rules?.max_players !== shown.max_players) {
    throw new Error(TERMS_CHANGED);
  }
  // A queued entry is held in an escrow and paid out automatically.
  if (!quote.enabled) throw new Error("Entries to this competition are unavailable right now; no ticket payment has been requested");
  const [eventResponse, keyResponse] = await Promise.all([
    fetch(`${oracleBase}/oracle/events/${competition.id}`),
    fetch(`${oracleBase}/oracle/pubkey`),
  ]);
  if (!eventResponse.ok || !keyResponse.ok) {
    throw new Error("The oracle's terms are unavailable; no ticket payment has been requested");
  }
  // Kept as text, so the wallet reads each line exactly as the oracle wrote it.
  const referenceEvent = await eventResponse.text();
  const { key } = await keyResponse.json();
  if (typeof key !== "string") throw new Error("The oracle returned no public key");
  return {
    entry_fee_sats: Number(form.dataset.entryFee),
    pool_rules: shown,
    oracle_pubkey: key,
    reference_event: referenceEvent,
  };
}

// The Lightning Address on the player's account: where automatic payouts,
// and Arkade refunds, are sent. Throws rather than dropping automatic payouts.
async function loadPayoutAddress() {
  const base = document.body.dataset.apiBase || "";
  const client = new AuthorizedClient(session.nostrClient, base);
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
    // The price shown on the form; the wallet refuses an invoice for anything but it and the
    // ticket's network fee.
    const shownPrice = {
      entryFee: Number(form.dataset.entryFee),
      ticketPrice: Number(form.dataset.ticketPrice),
      networkFee: Number(form.dataset.networkFee),
    };
    if (!Number.isSafeInteger(shownPrice.ticketPrice) || shownPrice.ticketPrice <= 0) {
      throw new Error("The competition is missing its ticket price");
    }
    if (!("networkFee" in form.dataset) || !Number.isSafeInteger(shownPrice.networkFee)) {
      throw new Error("The network fee estimate is unavailable right now; go back to the competitions list and open this competition again in a moment");
    }

    const body = document.body;
    const currentEntry = new Entry(body.dataset.apiBase || "", body.dataset.oracleBase || "", {
      id: competitionId,
    });
    await currentEntry.init();
    currentEntry.payoutTerms = payoutTerms;
    currentEntry.shownPrice = shownPrice;
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

// What the server charges for a ticket: the entry fee plus the coordinator fee,
// in basis points (250 = 2.5%), with exact halves rounded up like
// CoordinatorFee::fee_for. Older responses only carry a whole percent.
function ticketPriceSats(event) {
  const basisPoints =
    event.coordinator_fee_basis_points ?? Math.round(event.coordinator_fee_percentage * 100);
  return event.entry_fee + Math.floor((event.entry_fee * basisPoints + 5000) / 10000);
}

function formatSats(value) {
  return `${value.toLocaleString("en-US")} sats`;
}

// The ticket's total. Its entry and service fees must be the ones the form showed, and its
// network fee, fixed when the ticket was issued, at most twice the estimate the form showed:
// fee rates move, but the coordinator does not get to name any fee.
function ticketTotalSats(ticket, shown) {
  const fee = ticket.network_fee_sats;
  const total = shown.ticketPrice + fee;
  if (!Number.isSafeInteger(fee) || fee < 0 ||
      ticket.entry_fee_sats !== shown.entryFee ||
      ticket.entry_fee_sats + ticket.coordinator_fee_sats !== shown.ticketPrice ||
      ticket.ticket_price_sats !== total ||
      ticket.amount_sats !== total) {
    throw new Error(TERMS_CHANGED);
  }
  if (fee > 2 * shown.networkFee) {
    throw new Error(`The network fee rose to ${formatSats(fee)} since the form opened; go back to the competitions list and open this competition again to see the new price`);
  }
  return total;
}

// Replace the form's estimate with the ticket's own network fee.
function showNetworkFee(fee, total) {
  const $fee = document.getElementById("networkFee");
  if ($fee) $fee.textContent = formatSats(fee);
  const $total = document.getElementById("ticketTotal");
  if ($total) $total.textContent = formatSats(total);
}

function setupEntryForm() {
  document.addEventListener("click", (event) => {
    if (event.target.closest?.("#submitEntry")) submitEntry();
  });
}

