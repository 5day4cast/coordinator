import { WeatherData } from "./weather_data.js";
import { uuidv7 } from "https://unpkg.com/uuidv7@^1";
import { AuthorizedClient } from "./authorized_client.js";
import { getMusigRegistry } from "./main.js";

class Entry {
  constructor(
    coordinator_url,
    oracle_url,
    stations,
    competition,
    onSubmitSuccess,
  ) {
    this.weather_data = new WeatherData(oracle_url);
    this.coordinator_url = coordinator_url;
    this.client = new AuthorizedClient(window.nostrClient, coordinator_url);
    this.competition = competition;
    this.stations = stations;
    this.onSubmitSuccess = onSubmitSuccess;
    this.ticket = null;
  }

  async init() {
    return Promise.all([
      this.weather_data.get_competition_last_forecast(this.competition),
      this.setupEntry(),
    ]).then(([competition_forecasts, _]) => {
      this.competition_forecasts = competition_forecasts;

      for (const station_id in competition_forecasts) {
        const forecast = competition_forecasts[station_id];
        const option = {
          station_id: station_id,
          date: forecast["date"],
          temp_high: forecast["temp_high"],
          temp_low: forecast["temp_low"],
          wind_speed: forecast["wind_speed"],
        };
        this.entry["options"].push(option);
        this.entry["submit"][station_id] = {};
      }
    });
  }

  showEntry() {
    let $entryModal = document.getElementById("entry");
    this.clearEntry();

    let $entryValues = document.getElementById("entryContent");
    let $competitionId = document.createElement("h3");
    $competitionId.textContent = `Competition: ${this.competition.id}`;
    $entryValues.appendChild($competitionId);
    for (let option of this.entry["options"]) {
      let $stationDiv = document.createElement("div");
      if (option["station_id"]) {
        let $stationHeader = document.createElement("h5");
        $stationHeader.textContent = `Station: ${option.station_id}`;
        $stationHeader.classList.add("ml-2");
        $stationDiv.appendChild($stationHeader);
      }
      let $stationList = document.createElement("ul");
      if (option["wind_speed"]) {
        this.buildEntry(
          $stationList,
          option.station_id,
          "Wind Speed",
          "wind_speed",
          option["wind_speed"],
        );
      }
      if (option["temp_high"]) {
        this.buildEntry(
          $stationList,
          option.station_id,
          "High Temp",
          "temp_high",
          option["temp_high"],
        );
      }

      if (option["temp_low"]) {
        this.buildEntry(
          $stationList,
          option.station_id,
          "Low Temp",
          "temp_low",
          option["temp_low"],
        );
      }

      $stationDiv.appendChild($stationList);
      $entryValues.appendChild($stationDiv);
    }
    let $submitEntry = document.getElementById("submitEntry");
    $submitEntry.addEventListener("click", ($event) => {
      $event.target.classList.add("is-loading");
      this.submit($event);
    });

    if (!$entryModal.classList.contains("is-active")) {
      $entryModal.classList.add("is-active");
    }
    // Start ticket process with promise chain
    if (!this.canSubmitEntry) {
      this.handleTicketPayment()
        .then(() => {
          this.canSubmitEntry = true;
          $submitEntry.disabled = false;
          $paymentStatus.classList.remove("has-text-info");
          $paymentStatus.classList.add("has-text-success");
          $paymentStatus.textContent = "Payment received!";
        })
        .catch((error) => {
          console.error("Error handling ticket payment:", error);
          this.showError(error.message);
          this.hideEntry();
        });
    }
  }

  async handleTicketPayment() {
    const response = await this.client.get(
      `${this.coordinator_url}/competitions/${this.competition.id}/ticket`,
    );

    if (!response.ok) {
      throw new Error(`Failed to get ticket: ${response.status}`);
    }

    const ticketData = await response.json();
    this.ticket = {
      id: ticketData.ticket_id,
      payment_request: ticketData.payment_request,
    };

    return this.showPaymentModal();
  }

  showPaymentModal() {
    const $modal = document.getElementById("ticketPaymentModal");
    const $paymentRequest = document.getElementById("paymentRequest");
    const $copyButton = document.getElementById("copyInvoice");
    const $error = document.getElementById("ticketPaymentError");

    $paymentRequest.value = this.ticket.payment_request;
    $modal.classList.add("is-active");

    $copyButton.onclick = () => {
      navigator.clipboard.writeText(this.ticket.payment_request);
      $copyButton.textContent = "Copied!";
      setTimeout(() => {
        $copyButton.textContent = "Copy Invoice";
      }, 2000);
    };

    return new Promise((resolve, reject) => {
      const checkPayment = () => {
        this.client
          .get(
            `${this.coordinator_url}/competitions/${this.competition.id}/tickets/${this.ticket.id}/status`,
          )
          .then((response) => {
            if (!response.ok) {
              throw new Error(
                `Failed to check ticket status: ${response.status}`,
              );
            }
            return response.json();
          })
          .then((status) => {
            switch (status) {
              case "Paid":
                $modal.classList.remove("is-active");
                resolve();
                break;
              case "Expired":
                throw new Error(
                  "Ticket payment expired. Please request a new ticket.",
                );
              case "Used":
                throw new Error("Ticket has already been used.");
              case "Cancelled":
                throw new Error("Competition has been cancelled.");
              case "Reserved":
                // Continue polling
                setTimeout(checkPayment, 2000);
                break;
              default:
                throw new Error(`Unexpected ticket status: ${status}`);
            }
          })
          .catch((error) => {
            $error.textContent = error.message;
            $error.classList.remove("is-hidden");
            reject(error);
          });
      };

      checkPayment();
    });
  }

  async setupEntry() {
    const response = await this.client.get(`${this.coordinator_url}/entries`);

    if (!response.ok) {
      throw new Error(`Failed to fetch existing entries: ${response.status}`);
    }

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
      id: uuidv7(),
      competition_id: this.competition.id,
      submit: {},
      options: [],
      payout_hash,
      payout_preimage_encrypted: encryptedPayoutPreimage,
      ephemeral_pubkey: ephemeralPubkey,
      ephemeral_privatekey_encrypted: ephemeralPrivateKeyEncrypted,
    };
  }

  buildEntry($stationList, station_id, type_view, type, val) {
    let $optionListItem = document.createElement("li");
    // forecast val, observation val, val, score
    //$optionListItem.textContent = `Wind Speed ${} ${} ${} ${}`
    $optionListItem.classList.add("ml-4");
    $optionListItem.textContent = `${type_view}: `;
    let $breakdown = document.createElement("ul");

    let $forecast = document.createElement("li");
    $forecast.classList.add("ml-6");
    $forecast.textContent = `Forecast: ${val}`;
    $breakdown.appendChild($forecast);

    let $pick = document.createElement("li");
    $pick.classList.add("ml-6");
    this.buildEntryButtons($pick, station_id, type);
    $breakdown.appendChild($pick);

    $optionListItem.appendChild($breakdown);
    $stationList.appendChild($optionListItem);
  }

  buildEntryButtons($pick, station_id, weather_type) {
    let $overButton = document.createElement("button");
    $overButton.classList.add("button");
    $overButton.classList.add("is-info");
    $overButton.classList.add("is-outlined");
    $overButton.textContent = "Over";
    $overButton.id = `${station_id}_${weather_type}_over`;
    $overButton.addEventListener("click", ($event) => {
      this.handleEntryClick($event, station_id, weather_type, "over");
    });
    $pick.appendChild($overButton);

    // Create and append the "Par" button
    let $parButton = document.createElement("button");
    $parButton.textContent = "Par";
    $parButton.id = `${station_id}_${weather_type}_par`;
    $parButton.classList.add("button");
    $parButton.classList.add("is-primary");
    $parButton.classList.add("is-outlined");
    $parButton.addEventListener("click", ($event) => {
      this.handleEntryClick($event, station_id, weather_type, "par");
    });
    $pick.appendChild($parButton);

    // Create and append the "Under" button
    let $underButton = document.createElement("button");
    $underButton.textContent = "Under";
    $underButton.id = `${station_id}_${weather_type}_under`;
    $underButton.classList.add("button");
    $underButton.classList.add("is-link");
    $underButton.classList.add("is-outlined");
    $underButton.addEventListener("click", ($event) => {
      this.handleEntryClick($event, station_id, weather_type, "under");
    });
    $pick.appendChild($underButton);
  }

  hideEntry() {
    let $entryScoreModal = document.getElementById("entry");
    if ($entryScoreModal.classList.contains("is-active")) {
      $entryScoreModal.classList.remove("is-active");
    }
  }

  clearEntry() {
    let $entryValues = document.getElementById("entryContent");
    if ($entryValues) {
      while ($entryValues.firstChild) {
        $entryValues.removeChild($entryValues.firstChild);
      }
    }
  }

  handleEntryClick($event, station_id, weather_type, selected_val) {
    const $buttons = document.getElementsByTagName("button");
    const pattern = `${station_id}_${weather_type}`;
    $event.target.classList.toggle("is-active");
    $event.target.classList.toggle("is-outlined");

    for (let $button of $buttons) {
      if (
        $button.id.includes(pattern) &&
        $button.id != `${pattern}_${selected_val}`
      ) {
        $button.classList.remove("is-active");
        $button.classList.add("is-outlined");
      }
    }
    this.entry["submit"][station_id][weather_type] = selected_val;
  }

  async submit($event) {
    try {
      let submit = this.entry["submit"];
      let expected_observations = this.buildExpectedObservations(submit);

      const entry_body = {
        id: this.entry.id,
        ephemeral_pubkey: this.entry.ephemeral_pubkey,
        ephemeral_privatekey_encrypted:
          this.entry.ephemeral_privatekey_encrypted,
        payout_hash: this.entry.payout_hash,
        payout_preimage_encrypted: this.entry.payout_preimage_encrypted,
        event_id: this.competition.id,
        expected_observations: expected_observations,
      };

      console.log("Sending entry:", entry_body);

      const response = await this.client.post(
        `${this.coordinator_url}/entries`,
        entry_body,
      );

      if (!response.ok) {
        throw new Error(`Failed to create entry, status: ${response.status}`);
      }

      const responseData = await response.json();

      const registry = getMusigRegistry();
      if (registry) {
        await registry.createSession(
          window.taprootWallet,
          this.competition.id,
          this.entry.id,
          this.client,
          this.entryIndex,
        );
      }

      $event.target.classList.remove("is-loading");
      this.showSuccess();
    } catch (e) {
      console.error("Error submitting entry:", e);
      $event.target.classList.remove("is-loading");
      this.showError(e.message);
      if (this.onSubmitSuccess) {
        this.onSubmitSuccess();
      }
    }
  }

  showSuccess() {
    let $success = document.getElementById("successMessage");
    $success.classList.remove("hidden");
    setTimeout(() => {
      $success.classList.add("hidden");
      this.hideEntry();
      this.clearEntry();
    }, 600);
  }

  showError(msg) {
    let $error = document.getElementById("errorMessage");
    $error.textContent = msg;
    $error.classList.remove("hidden");

    setTimeout(() => {
      $error.classList.add("hidden");
      this.hideEntry();
      this.clearEntry();
    }, 600);
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
    const valueMap = {
      par: "Par",
      over: "Over",
      under: "Under",
    };

    if (!(raw_select in valueMap)) {
      throw new Error(`Failed to match selected option value: ${raw_select}`);
    }

    return valueMap[raw_select];
  }
}

export { Entry };
