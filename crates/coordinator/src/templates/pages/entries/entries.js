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
    // If oracle URL is a mock placeholder, extract forecasts from the rendered form
    if (!this.oracle_url || this.oracle_url.startsWith("mock://")) {
      return this.getForecastsFromForm();
    }

    const response = await fetch(
      `${this.oracle_url}/oracle/events/${this.competition.id}/forecasts/latest`,
    );
    if (!response.ok)
      throw new Error(`Failed to fetch forecasts: ${response.status}`);
    return response.json();
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
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/entries`,
    );
    if (!response.ok)
      throw new Error(`Failed to fetch existing entries: ${response.status}`);

    const existingEntries = await response.json();
    existingEntries.sort((a, b) => a.id.localeCompare(b.id));

    this.entryIndex = existingEntries.length;
    const payout_hash = await window.taprootWallet.addEntryIndex(
      this.entryIndex,
    );
    const nostrPubkey = await window.nostrClient.getPublicKey();
    const encryptedPayoutPreimage =
      await window.taprootWallet.getEncryptedDlcPayoutPreimage(
        this.entryIndex,
        nostrPubkey,
      );
    const ephemeralPrivateKeyEncrypted =
      await window.taprootWallet.getEncryptedDlcPrivateKey(
        this.entryIndex,
        nostrPubkey,
      );
    const ephemeralPubkey = await window.taprootWallet.getDlcPublicKey(
      this.entryIndex,
    );

    this.entry = {
      id: generateUuidV7(),
      competition_id: this.competition.id,
      submit: {},
      options: [],
      payout_hash,
      payout_preimage_encrypted: encryptedPayoutPreimage,
      ephemeral_pubkey: ephemeralPubkey,
      ephemeral_privatekey_encrypted: ephemeralPrivateKeyEncrypted,
    };
  }

  async handleTicketPayment(btc_pubkey) {
    const response = await this.client.post(
      `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/ticket`,
      { btc_pubkey },
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
    };

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
      const handleClose = () => {
        cleanup();
        reject(new Error("Payment cancelled by user"));
      };

      $modal
        .querySelector(".modal-background")
        .addEventListener("click", handleClose, { once: true });
      $modal
        .querySelector(".modal-close")
        .addEventListener("click", handleClose, { once: true });

      $qrCode.callback = async () => {
        try {
          const response = await this.client.get(
            `${this.coordinator_url}/api/v1/competitions/${this.competition.id}/tickets/${this.ticket.id}/status`,
          );

          if (!response.ok)
            throw new Error(
              `Failed to check ticket status: ${response.status}`,
            );

          const status = await response.json();

          if (status === "Settled" || status === "Paid") {
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
          $error.textContent = error.message;
          $error.classList.remove("is-hidden");
          updateStatus("Payment failed", "danger");
          cleanup();
          reject(error);
          return false;
        }
      };
    });
  }

  async submit(expectedObservations) {
    try {
      await this.handleTicketPayment(this.entry.ephemeral_pubkey);

      let encrypted_keymeld_private_key = null;
      let keymeld_auth_pubkey = null;

      if (
        this.ticket.keymeld_session_id &&
        this.ticket.keymeld_enclave_public_key
      ) {
        const ephemeralPrivateKey = await window.taprootWallet.getDlcPrivateKey(
          this.entryIndex,
        );
        encrypted_keymeld_private_key = await window.encryptToEnclave(
          ephemeralPrivateKey,
          this.ticket.keymeld_enclave_public_key,
        );
        keymeld_auth_pubkey = await window.deriveKeymeldAuthPubkey(
          ephemeralPrivateKey,
          this.ticket.keymeld_session_id,
        );
      }

      const entry_body = {
        id: this.entry.id,
        ephemeral_pubkey: this.entry.ephemeral_pubkey,
        ephemeral_privatekey_encrypted:
          this.entry.ephemeral_privatekey_encrypted,
        payout_hash: this.entry.payout_hash,
        payout_preimage_encrypted: this.entry.payout_preimage_encrypted,
        event_id: this.competition.id,
        ticket_id: this.ticket.id,
        expected_observations: expectedObservations,
        encrypted_keymeld_private_key,
        keymeld_auth_pubkey,
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

async function deriveKeymeldAuthPubkey(privateKeyHex, sessionId) {
  if (typeof window.derive_keymeld_auth_pubkey === "function") {
    return window.derive_keymeld_auth_pubkey(privateKeyHex, sessionId);
  }
  throw new Error("Keymeld WASM not loaded");
}

async function encryptToEnclave(privateKeyHex, enclavePubkeyHex) {
  if (typeof window.encrypt_private_key_for_enclave === "function") {
    return window.encrypt_private_key_for_enclave(
      privateKeyHex,
      enclavePubkeyHex,
    );
  }
  throw new Error("Keymeld WASM not loaded");
}

window.deriveKeymeldAuthPubkey = deriveKeymeldAuthPubkey;
window.encryptToEnclave = encryptToEnclave;

// Current entry instance for the form
let currentEntry = null;

/**
 * Handle pick button selection (Over/Par/Under)
 * Called when user clicks a prediction button
 */
function selectPick(button) {
  const field = button.dataset.field;
  const value = button.dataset.value;

  // Find all buttons in this group and deselect them
  const group = button.closest(".buttons");
  group.querySelectorAll(".pick-button").forEach((btn) => {
    btn.classList.remove("is-info", "is-active");
    btn.classList.add("is-outlined");
  });

  // Select clicked button
  button.classList.remove("is-outlined");
  button.classList.add("is-info", "is-active");

  // Update hidden input
  const hiddenInput = document.getElementById(field);
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

    // Validate we have picks
    if (Object.keys(picks).length === 0) {
      throw new Error("Please make at least one prediction");
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
    errorMsg.textContent = error.message || "Failed to submit entry";
    errorMsg.classList.remove("hidden");
    submitBtn.disabled = false;
    submitBtn.classList.remove("is-loading");
  }
}

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
