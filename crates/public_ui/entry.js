import { WeatherData } from "./weather_data.js";
import { uuidv7 } from "https://unpkg.com/uuidv7@^1";
import { hash_object } from "./utils.js";

class Entry {
  constructor(coordinator_url, oracle_url, stations, competition) {
    this.weather_data = new WeatherData(oracle_url);
    this.coordinator_url = coordinator_url;
    this.competition = competition;
    this.stations = stations;
    let preimage = generateRandomPreimage();
    let preimage_hash = (this.entry = {
      competition_id: this.competition.id,
      submit: {},
      payout_preimage: preimageToHex(preimage),
      payout_hash: sha256(preimage),
    });
  }

  async init() {
    /*return Promise.all([
      this.weather_data.get_competition_last_forecast(this.competition),
      generateKeyPair(),
    ]).then(([competition_forecasts, key_pair]) => {
      this.competition_forecasts = competition_forecasts;
      this.entry["options"] = [];
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
        this.entry.ephemeral_keys = key_pair;
      }
    });*/
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
    let xonly_pubkey_hex = await window.nostrClient.getPublicKey();
    let submit = this.entry["submit"];
    let expected_observations = build_expected_observations_from_submit(submit);
    let competition_id = this.entry.competition_id;

    let encrypted_private_key = await window.nostrClient.nip44.encrypt(
      xonly_pubkey_hex,
      this.entry.ephemeral_keys.privateKey,
    );
    let encrypted_payout_preimage = await window.nostrClient.nip44.encrypt(
      xonly_pubkey_hex,
      this.entry.payout_preimage,
    );

    let entry_body = {
      id: uuidv7(),
      pubkey: xonly_pubkey_hex,
      ephemeral_private_key: encrypted_private_key, // encrypt to nostr pubkey via nip 04
      ephemeral_pubkey: this.entry.ephemeral_keys.publicKey, // needs to be associated to an ephemeral private key as this may be exposed to the market maker on payout
      payout_preimage: encrypted_payout_preimage, // encrypt to nostr pubkey via nip 04
      payout_hash: this.entry.payout_hash, // needs to be ephemeral preimage, used to get the payout
      event_id: competition_id,
      expected_observations: expected_observations,
    };

    /* broadcasted to nostr relays so user has ability to complete the payout even if coordinator is gone
    TODO: need to add the ability to publish to a list of relays, user defined with sane defaults
    let nostr_store_data = {
      event_id: competition_id,
      entry_id: entry_body.id,
      ephemeral_pubkey: this.entry.ephemeral_keys.publicKey,
      ephemeral_private_key: encrypted_private_key, // encrypt to nostr pubkey via nip 04
      payout_preimage: encrypted_payout_preimage, // encrypt to nostr pubkey via nip 04
      payout_hash: this.entry.payout_hash,
    };
    console.log("Sending nostr backup:", nostr_store_data);
    */ m;

    console.log("Sending entry:", entry_body);

    const headers = {
      "Content-Type": "application/json",
    };
    fetch(`${this.coordinator_url}/entries`, {
      method: "POST",
      headers: headers,
      body: JSON.stringify(entry_body),
    })
      .then((response) => {
        if (!response.ok) {
          console.error(response);
          $event.target.classList.remove("is-loading");
          this.showError(`Failed to create entry, status: ${response.status}`);
        } else {
          console.log("entry: ", this.entry);
          $event.target.classList.remove("is-loading");
          this.showSuccess();
        }
      })
      .catch((e) => {
        console.error("Error submitting entry: {}", e);
      });
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
}

function build_expected_observations_from_submit(submit) {
  console.log(submit);
  let expected_observations = [];
  for (let [station_id, choices] of Object.entries(submit)) {
    let stations = {
      stations: station_id,
    };
    for (let [weather_type, selected_val] of Object.entries(choices)) {
      stations[weather_type] = convert_select_val(selected_val);
    }
    expected_observations.push(stations);
  }
  return expected_observations;
}

function convert_select_val(raw_select) {
  switch (raw_select) {
    case "par":
      return "Par";
    case "over":
      return "Over";
    case "under":
      return "Under";
    default:
      throw new Error(`Failed to match selected option value: ${raw_select}`);
  }
}

export { Entry };
