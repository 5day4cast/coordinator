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
    // Set while its invoice waits to be paid, and once it is.
    this.awaitingPayment = false;
    this.paid = false;
  }

  // `kept` is the entry an earlier ticket request carried (see keptEntry). It is used again
  // unless another account's wallet made it.
  async init(kept = null) {
    // The entry key is derived from the entry id, so every entry gets its own
    // key and no counter or entry ordering is involved.
    // An unpaid ticket the coordinator lists for this account (`kept` without a key) is the
    // account's own: its key is the one this wallet derives from its id.
    const reuse = kept && (kept.ephemeral_pubkey === undefined ||
      session.dlcWallet.entryRegistration(kept.id).ephemeral_pubkey === kept.ephemeral_pubkey);
    const id = reuse ? kept.id : session.wasm.DlcWallet.newEntryId();
    const { ephemeral_pubkey, payout_hash } =
      session.dlcWallet.entryRegistration(id);
    keepEntry(this.competition.id, { id, ephemeral_pubkey });

    this.entry = {
      id,
      competition_id: this.competition.id,
      submit: {},
      payout_hash,
      ephemeral_pubkey,
    };
  }

  async handleTicketPayment(btc_pubkey) {
    let response;
    try {
      response = await this.client.post(
        `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/ticket`,
        { btc_pubkey, payout: this.payoutChoice },
      );
    } catch (error) {
      // The coordinator says why it refused: already entered, entries closed, full.
      throw await requestFailure(error);
    }

    const ticketData = await response.json();
    // Until the wallet accepts this ticket: one it refuses (other terms, a higher network fee)
    // would come back the same for this entry, so the next Pay starts a new entry.
    this.renew = true;
    this.ticket = {
      id: ticketData.ticket_id,
      payment_request: ticketData.payment_request,
      invoice_expires_at: ticketData.invoice_expires_at ?? null,
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
    this.renew = false;
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
  // polling, hidden, since a payment from a wallet may still arrive; the
  // coordinator fails an unpaid ticket after 10 minutes. `onDialogClosed`
  // hears the dialog close while the ticket is unpaid; `reopenPayment` shows
  // the same invoice again.
  async showPaymentModal() {
    const $modal = document.getElementById("ticketPaymentModal");
    const ticketId = this.ticket.id;
    $modal.dataset.ticketId = ticketId;
    const ownsModal = () => $modal.dataset.ticketId === ticketId;
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
    const $qrBadge = document.createElement("span");
    $qrBadge.className = "payment-qr-badge";
    $qrBadge.textContent = "Tap to copy";
    $qrButton.append($qrCode, $qrBadge);
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

    const hint = "Tap the QR code to copy the invoice";
    $copyFeedback.textContent = hint;
    const copyInvoice = async () => {
      try {
        await navigator.clipboard.writeText(invoice);
      } catch (err) {
        // Older iOS Safari: copy from a hidden textarea instead. It goes in
        // the modal so focus stays inside the dialog.
        const $text = document.createElement("textarea");
        $text.value = invoice;
        $text.readOnly = true;
        $text.className = "payment-copy-buffer";
        $modal.append($text);
        $text.select();
        const copied = document.execCommand("copy");
        $text.remove();
        if (!copied) {
          console.error("Failed to copy:", err);
          $copyFeedback.textContent = "Could not copy; use a wallet button below";
          return;
        }
      }
      $copyFeedback.textContent = "✓ Invoice copied";
      setTimeout(() => ($copyFeedback.textContent = hint), 2000);
    };
    $qrButton.onclick = copyInvoice;
    // One all-in number; what it is made of is on the form only.
    document.getElementById("ticketPaymentAmount").textContent =
      `Pay ${formatSats(this.ticketAmountSats)} by Lightning to enter this competition.`;
    const stopCountdown = countDownTo(this.ticket.invoice_expires_at, document.getElementById("ticketPaymentExpiry"));

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
    this.awaitingPayment = true;
    openModal($modal);

    return new Promise((resolve, reject) => {
      let finished = false;
      // Closing (the backdrop, Esc, the close button) only hides the invoice.
      const closed = () => { if (ownsModal()) this.onDialogClosed?.(); };
      const finish = (error) => {
        if (finished) return;
        finished = true;
        stopCountdown();
        document.removeEventListener("fw:ticket-paid", paid);
        document.removeEventListener("fw:ticket-failed", failed);
        $modal.removeEventListener("fw:modal-closed", closed);
        clearTimeout(giveUp);
        this.awaitingPayment = false;
        if (ownsModal()) {
          clearPaymentModal($modal, $qrContainer);
        }
        if (error) {
          // The ticket expired or failed: the next Pay starts a new entry.
          this.renew = true;
          $error.textContent = error.message;
          $error.classList.remove("is-hidden");
          reject(error);
        } else {
          this.paid = true;
          this.onPaid?.();
          resolve(true);
        }
      };
      const paid = (event) => { if (event.detail?.ticket_id === ticketId) finish(); };
      const failed = (event) => {
        if (event.detail?.ticket_id === ticketId) {
          finish(new Error(event.detail.message || "The ticket payment failed"));
        }
      };
      // Only if the coordinator never answers: it fails an unpaid ticket after 10 minutes.
      const giveUp = setTimeout(
        () => finish(new Error("The payment wasn't confirmed; check your wallet, then try again")),
        15 * 60 * 1000,
      );
      document.addEventListener("fw:ticket-paid", paid);
      document.addEventListener("fw:ticket-failed", failed);
      $modal.addEventListener("fw:modal-closed", closed);
    });
  }

  // The invoice of the ticket already issued, shown again after its dialog was closed.
  reopenPayment() {
    const modal = document.getElementById("ticketPaymentModal");
    if (modal?.dataset.ticketId === this.ticket.id) openModal(modal);
  }

  // Pays for the ticket, unless it is paid already, then enters the picks as they are now
  // (`this.entry.submit`, updated when Pay reopens the invoice).
  async submit() {
    try {
      if (!this.paid) await this.handleTicketPayment(this.entry.ephemeral_pubkey);
      const expectedObservations = this.buildExpectedObservations(this.entry.submit);

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

      let response;
      try {
        response = await this.client.post(`${this.coordinator_url}/api/v1/entries`, entry_body);
      } catch (error) {
        throw await requestFailure(error);
      }
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


// "Invoice expires in 12:34" in `element`, every second until `expiresAt` (RFC 3339) or until
// the returned function stops it. Nothing when the ticket carries no expiry.
function countDownTo(expiresAt, element) {
  const end = expiresAt ? Date.parse(expiresAt) : NaN;
  if (!element) return () => {};
  if (Number.isNaN(end)) {
    element.textContent = "";
    return () => {};
  }
  const tick = () => {
    const seconds = Math.max(0, Math.floor((end - Date.now()) / 1000));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor(seconds / 60) % 60;
    const clock = `${hours ? `${hours}:${String(minutes).padStart(2, "0")}` : minutes}:${String(seconds % 60).padStart(2, "0")}`;
    element.textContent = seconds > 0 ? `Invoice expires in ${clock}` : "Invoice expired";
  };
  tick();
  const timer = setInterval(tick, 1000);
  return () => clearInterval(timer);
}

// Picks by station from the form's checked radios, named `KPWM_temp_high`.
// A reading with no radio checked is skipped.
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

// Pay while signed out: log in first. The wallet starts loading now, not
// when the form is shown.
function showLogin() {
  openModal(document.getElementById("loginModal"));
  loadWallet();
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

// Messages for requests that never got an answer, or got a server error: things the
// player can act on, never a bare status.
const UNREACHABLE = "Couldn't reach the server, try again";
const SERVER_ERROR = "Something went wrong on the server; try again in a moment";

// A fetch that never reached the server: offline, or the server restarting. Browsers word
// it differently (Chrome and Brave, Firefox, Safari, Node).
function unreachable(detail) {
  return /Failed to fetch|NetworkError|Load failed|fetch failed/i.test(detail);
}

// What a failed coordinator request (AuthorizedClient's error, with its response) tells the
// player: the coordinator's own reason for a refusal; for a 503 (the oracle, the network fee
// estimate or a busy database), to try again in a moment.
async function requestFailure(error) {
  const response = error?.response;
  if (!response) return error;
  let data = null;
  try {
    data = await response.json();
  } catch {
    // No JSON body: a proxy's page, say.
  }
  const reason = typeof data?.error === "string" ? data.error : "";
  const sentence = reason && reason[0].toUpperCase() + reason.slice(1);
  if (response.status === 503) {
    if (!sentence) return new Error("The server is busy; try again in a moment");
    return new Error(/try again/i.test(sentence) ? sentence : `${sentence}; try again in a moment`);
  }
  // A proxy with nothing behind it, during a restart.
  if (response.status === 502 || response.status === 504) return new Error(UNREACHABLE);
  if (response.status >= 500) return new Error(SERVER_ERROR);
  return sentence ? new Error(sentence) : error;
}

// The Pay button while a request or the invoice is out. The form may have been loaded again
// since, so callers that run later look the button up then.
function setBusy(button, busy) {
  if (!button) return;
  button.disabled = busy;
  if (busy) button.classList.add("is-loading");
  else button.classList.remove("is-loading");
}

// Pay with nothing picked: say so above the picks, and take the player to the first one.
// Without the picks on the page (forecasts still loading), the message goes under Pay.
function askForPicks(form, errorMsg) {
  const message = document.getElementById("picksMessage") ?? errorMsg;
  const count = Number(form.dataset.maxValues);
  const rows = Number(form.dataset.pickRows) || count;
  const any = count < rows ? `, any ${count} of the ${rows} rows,` : "";
  message.textContent = `Make exactly ${count} ${count === 1 ? "pick" : "picks"}${any} before paying.`;
  message.classList.remove("hidden");
  const first = form.querySelector?.(`${PICK}:not(:disabled)`);
  first?.focus();
}

function hidePicksMessage() {
  document.getElementById("picksMessage")?.classList.add("hidden");
}

// The counter above Pay, for `made` picks of the `need` a competition with `rows` rows takes.
// Before any pick it reads as the server rendered it (`picks_to_make` in mod.rs).
function picksLeft(made, need, rows) {
  const picks = (n) => `${n} ${n === 1 ? "pick" : "picks"}`;
  if (made === 0) {
    return need < rows ? `${picks(need)} to make: any ${need} of the ${rows} rows.`
      : `${picks(need)} to make: one in every row.`;
  }
  if (made < need) return `${need - made} more to pick: ${made} of ${need} made.`;
  if (made === need) return `All ${picks(need)} made.`;
  return `${picks(made - need)} too many: take ${made - need} back to make exactly ${need}.`;
}

function showPicksLeft(form) {
  const counter = document.getElementById("picksLeft");
  if (!counter || !form) return;
  const need = parseInt(form.dataset.maxValues, 10) || 1;
  const rows = parseInt(form.dataset.pickRows, 10) || need;
  const made = Object.values(collectPicks(form)).reduce((n, station) => n + Object.keys(station).length, 0);
  counter.textContent = picksLeft(made, need, rows);
}

// The entry every ticket request for a competition carries, by competition id, until it is
// entered or its ticket expires, fails or is refused. The coordinator takes the entry id as the
// request's idempotency key: a request for the same entry gets the same ticket and invoice, so
// a retry after an answer that never came goes through. The tab keeps it as well, so a reload
// within the ticket's reservation asks for the same ticket.
const keptEntries = new Map();
const keptEntryKey = (competitionId) => `fw:entry:${competitionId}`;

function keptEntry(competitionId) {
  if (!keptEntries.has(competitionId)) {
    try {
      const kept = JSON.parse(sessionStorage.getItem(keptEntryKey(competitionId)));
      if (kept) keptEntries.set(competitionId, kept);
    } catch {
      // No session storage (a private window, say): kept for this page only.
    }
  }
  return keptEntries.get(competitionId) ?? null;
}

function keepEntry(competitionId, entry) {
  keptEntries.set(competitionId, entry);
  try {
    sessionStorage.setItem(keptEntryKey(competitionId), JSON.stringify(entry));
  } catch {}
}

function forgetEntry(competitionId) {
  keptEntries.delete(competitionId);
  try {
    sessionStorage.removeItem(keptEntryKey(competitionId));
  } catch {}
}

// The entry whose ticket was issued and which isn't entered yet. Pay picks it up again rather
// than asking for another ticket: its invoice while that is unpaid, the entry itself once paid.
let pendingEntry = null;
// Admission is synchronous: navigating while terms load must not start another ticket.
let submissionBusy = false;

function clearPaymentModal(modal, qrContainer) {
  const idle = document.createElement("div");
  idle.id = "paymentStatus";
  document.getElementById("paymentStatus")?.replaceWith(idle);
  qrContainer.replaceChildren();
  document.querySelectorAll("#walletLinks a").forEach((a) => a.removeAttribute("href"));
  closeModal(modal);
  delete modal.dataset.ticketId;
}

function currentPayButton(competitionId) {
  const form = document.getElementById("entryForm");
  return form?.dataset.competitionId === competitionId ? document.getElementById("submitEntry") : null;
}

function showEntrySuccess(competitionId) {
  const button = currentPayButton(competitionId);
  if (!button) return;
  // The paid ticket the form showed is entered now.
  const paidNotice = document.getElementById("entryPaid");
  if (paidNotice?.dataset.ticketId) {
    delete paidNotice.dataset.ticketId;
    paidNotice.replaceChildren?.();
  }
  document.getElementById("errorMessage")?.classList.add("hidden");
  document.getElementById("successMessage")?.classList.remove("hidden");
  button.textContent = "Entered";
  button.disabled = true;
  button.classList.remove("is-loading");
  button.classList.add("is-success");
}

function showEntryFailure(competitionId, message) {
  const button = currentPayButton(competitionId);
  if (!button) return;
  const error = document.getElementById("errorMessage");
  if (error) {
    error.textContent = message;
    error.classList.remove("hidden");
  }
  setBusy(button, false);
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
  const competitionId = form.dataset.competitionId;
  const submitBtn = document.getElementById("submitEntry");
  const errorMsg = document.getElementById("errorMessage");
  const successMsg = document.getElementById("successMessage");

  errorMsg.classList.add("hidden");
  errorMsg.textContent = "";
  successMsg.classList.add("hidden");
  hidePicksMessage();

  // The picks first: nothing to log in or pay for without them.
  const picks = collectPicks(form);
  let choiceCount = 0;
  for (const stationPicks of Object.values(picks)) {
    choiceCount += Object.keys(stationPicks).length;
  }
  if (choiceCount === 0) {
    askForPicks(form, errorMsg);
    return;
  }
  const maxValues = parseInt(form.dataset.maxValues, 10) || 1;
  if (choiceCount !== maxValues) {
    errorMsg.textContent = `Make exactly ${maxValues} ${maxValues === 1 ? "pick" : "picks"}; you made ${choiceCount}.`;
    errorMsg.classList.remove("hidden");
    return;
  }

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

  // A ticket already issued for this competition: show its invoice again, with the picks as
  // they are now. The call that issued it still waits for the payment and enters the picks
  // once it arrives.
  if (pendingEntry && pendingEntry.competition.id !== competitionId) {
    errorMsg.textContent = "Finish your pending entry in the other competition before starting another payment.";
    errorMsg.classList.remove("hidden");
    if (pendingEntry.awaitingPayment) pendingEntry.reopenPayment();
    return;
  }
  const pending = pendingEntry;
  if (pending?.awaitingPayment) {
    pending.entry.submit = picks;
    setBusy(submitBtn, true);
    pending.reopenPayment();
    return;
  }

  if (submissionBusy) {
    errorMsg.textContent = "Your entry is still being prepared or submitted. Please wait for it to finish.";
    errorMsg.classList.remove("hidden");
    return;
  }

  submissionBusy = true;
  setBusy(submitBtn, true);
  const payButton = () => currentPayButton(competitionId);

  try {
    let currentEntry = pending;
    const paid = currentEntry ? null : paidTicket(competitionId);
    if (currentEntry) {
      // Paid, but the entry didn't go through: enter it again, never pay again.
      currentEntry.entry.submit = picks;
    } else if (paid) {
      // Paid before the page reloaded, never entered: enter it, never pay again.
      currentEntry = paidEntry(form, picks, paid);
      pendingEntry = currentEntry;
    } else {
      currentEntry = await newEntry(form, picks, takeUnpaidTicket(competitionId));
      pendingEntry = currentEntry;
    }
    try {
      await enter(currentEntry, payButton);
    } catch (refused) {
      // This page didn't know of the player's unpaid entries (opened before they were made, or
      // in another tab): pay the oldest of them instead of starting another.
      const unpaid = refused?.message === TOO_MANY_UNPAID && !currentEntry.paid && !currentEntry.awaitingPayment
        ? await oldestUnpaidTicket(currentEntry) : null;
      if (!unpaid) throw refused;
      currentEntry = await newEntry(form, picks, unpaid);
      pendingEntry = currentEntry;
      await enter(currentEntry, payButton);
    }
    pendingEntry = null;
    forgetEntry(currentEntry.competition.id);

    showEntrySuccess(competitionId);
  } catch (caught) {
    console.error("Entry submission failed:", caught);
    // A ticket that failed or was never paid is done with; a paid one is entered on the next Pay.
    // The entry itself is kept for the next request unless its ticket expired, failed or was
    // refused: a request whose answer never came gets the same ticket back.
    if (pendingEntry && !pendingEntry.paid && !pendingEntry.awaitingPayment) {
      if (pendingEntry.renew) forgetEntry(pendingEntry.competition.id);
      pendingEntry = null;
    }

    const error = caught?.response ? await requestFailure(caught) : caught;
    // WASM rejects with plain strings, which have no message property.
    const detail = typeof error === "string" ? error : typeof error?.message === "string" ? error.message : "";
    let userMessage = detail || "Failed to submit entry";
    if (detail.includes("No signer initialized")) {
      userMessage = "Session expired. Please log in again.";
      showLogin();
    } else if (unreachable(detail)) {
      userMessage = UNREACHABLE;
    }

    showEntryFailure(competitionId, userMessage);
  } finally {
    submissionBusy = false;
  }
}

// Pays for `entry`'s ticket and enters it, keeping Pay in step with the payment dialog.
async function enter(entry, payButton) {
  // The dialog closed with the ticket unpaid: Pay works again, and reopens it.
  entry.onDialogClosed = () => setBusy(payButton(), false);
  entry.onPaid = () => setBusy(payButton(), true);
  await entry.submit();
}

// The coordinator's refusal when the player already holds as many unpaid tickets in a queued
// competition as one may (`queued::TOO_MANY_UNPAID`).
const TOO_MANY_UNPAID = "You have unpaid entries waiting in this competition; open its entry form and press Pay to pay one, or wait for its invoice to expire";

// The unpaid ticket the form says the player holds (the `entryUnpaid` notice, rendered for the
// signed-in player), as the entry to resume: a queued ticket's id is its entry's, and the entry
// key is derived from the id. Taken once: if it expires or fails, the next Pay starts afresh.
function takeUnpaidTicket(competitionId) {
  const notice = document.getElementById("entryUnpaid");
  const id = notice?.dataset.ticketId;
  if (!id || document.getElementById("entryForm")?.dataset.competitionId !== competitionId) return null;
  delete notice.dataset.ticketId;
  notice.replaceChildren?.();
  return { id };
}

// The oldest unpaid ticket the player holds in `entry`'s competition, asked of the coordinator.
async function oldestUnpaidTicket(entry) {
  try {
    const response = await entry.client.get(
      `${entry.coordinator_url}/api/v1/competitions/${entry.competition.id}/tickets/unpaid`,
    );
    const [oldest] = await response.json();
    return oldest?.ticket_id ? { id: oldest.ticket_id } : null;
  } catch (error) {
    console.error("Unpaid tickets could not be listed:", error);
    return null;
  }
}

// The paid ticket the form says the player holds (the `entryPaid` notice, rendered for the
// signed-in player): paid for, but its entry never went in, as when the page reloaded first.
// Entering it pays nothing: the entry goes in under the ticket's entry id with the key it was
// paid with, and the coordinator uses the Keymeld registration sent before paying. Once taken,
// a failed entry is retried as the pending one.
function paidTicket(competitionId) {
  const notice = document.getElementById("entryPaid");
  const { ticketId, entryId, entryKey } = notice?.dataset ?? {};
  if (!ticketId || !entryId || document.getElementById("entryForm")?.dataset.competitionId !== competitionId) return null;
  return { ticket_id: ticketId, entry_id: entryId, ephemeral_pubkey: entryKey };
}

// The entry for `paid`, the player's paid ticket, with `picks`. The wallet derives the entry key
// from the entry id; a key other than the one the ticket was paid with is another wallet's.
function paidEntry(form, picks, paid) {
  const { ephemeral_pubkey, payout_hash } = session.dlcWallet.entryRegistration(paid.entry_id);
  if (paid.ephemeral_pubkey && ephemeral_pubkey !== paid.ephemeral_pubkey) {
    throw new Error("This entry was paid for from another account; log in with that account to finish it");
  }
  const body = document.body;
  const entry = new Entry(body.dataset.apiBase || "", body.dataset.oracleBase || "", {
    id: form.dataset.competitionId,
  });
  entry.entry = { id: paid.entry_id, competition_id: entry.competition.id, submit: picks, payout_hash, ephemeral_pubkey };
  entry.ticket = { id: paid.ticket_id };
  entry.paid = true;
  entry.preparedRegistration = null;
  return entry;
}

// Pay's label: "Enter" while the form shows a paid ticket to enter, the price otherwise. A busy
// or finished button keeps its own.
function labelPay() {
  const button = document.getElementById("submitEntry");
  const label = button?.dataset.payLabel;
  if (!label || button.disabled || ![label, "Enter"].includes(button.textContent.trim())) return;
  button.textContent = document.getElementById("entryPaid")?.dataset.ticketId ? "Enter" : label;
}

// A new entry for `picks`, checked against the terms the form showed, with its payout address.
// `unpaid` is an unpaid ticket of the player's to pay instead of starting a new entry.
async function newEntry(form, picks, unpaid = null) {
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
    throw new Error("The entry fee is unavailable right now; go back to the competitions list and open this competition again in a moment");
  }

  const body = document.body;
  const currentEntry = new Entry(body.dataset.apiBase || "", body.dataset.oracleBase || "", {
    id: form.dataset.competitionId,
  });
  await currentEntry.init(unpaid ?? keptEntry(form.dataset.competitionId));
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
  return currentEntry;
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
    throw new Error(`The entry fee rose to ${formatSats(total)} since the form opened; go back to the competitions list and open this competition again to see the new price`);
  }
  return total;
}

// Replace the form's estimate with the ticket's own network fee, in the total and on Pay.
function showNetworkFee(fee, total) {
  const $fee = document.getElementById("networkFee");
  if ($fee) $fee.textContent = formatSats(fee);
  const $total = document.getElementById("ticketTotal");
  if ($total) $total.textContent = formatSats(total);
  const $pay = document.getElementById("submitEntry");
  if ($pay) $pay.textContent = `Pay ${formatSats(total)} and enter`;
}

const PICK = ".pick-option input[type=radio]";

// A pick's radio button takes the pick back when chosen again, by tap or click: the input
// chosen last in its row carries `data-picked`, which the browser's own checking doesn't
// touch. Arrow keys move the pick within the row as usual.
function togglePick(input) {
  if (input.dataset.picked) {
    input.checked = false;
    delete input.dataset.picked;
    return;
  }
  for (const other of input.form?.querySelectorAll(`input[name="${CSS.escape(input.name)}"]`) ?? []) {
    delete other.dataset.picked;
  }
  input.dataset.picked = "1";
}

// Space on a chosen pick takes it back too. Browsers send no click for Space on a radio that
// is already checked, so the key does it here; its default is stopped, or the key's release
// would check the radio again.
function unpickWithSpace(event) {
  const input = event.target;
  if (event.key !== " " || !input?.matches?.(PICK) || !input.dataset.picked) return;
  event.preventDefault();
  togglePick(input);
  showPicksLeft(input.form);
}

function setupEntryForm() {
  document.addEventListener("click", (event) => {
    if (event.target.closest?.("#submitEntry")) submitEntry();
    if (event.target instanceof HTMLInputElement && event.target.matches(PICK)) {
      togglePick(event.target);
      hidePicksMessage();
      showPicksLeft(event.target.form);
    }
  });
  document.addEventListener("keydown", unpickWithSpace);
  // The paid notice comes and goes with a log-in or log-out.
  document.addEventListener("htmx:after:swap", labelPay);
  // Arrow keys move a pick within its row without a click.
  document.addEventListener("change", (event) => {
    if (event.target?.matches?.(PICK)) showPicksLeft(event.target.form);
  });
}
