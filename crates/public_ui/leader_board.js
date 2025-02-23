import { WeatherData } from "./weather_data.js";

//TODO:
// highlight the entries that placed/won money when competition is completed
class LeaderBoard {
  constructor(coordinator_url, oracle_url, competition) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.competition = competition;
    this.weather_data = new WeatherData(oracle_url);
  }

  async init() {
    this.clearTable();
    // if we get no readings during the competition window
    // we should cancel the competition and refund people
    Promise.all([
      this.getEntries(this.competition),
      this.getReadings(this.competition),
      this.weather_data.get_competition_last_forecast(this.competition),
    ]).then(([entries, observations, lastForecasts]) => {
      const entryScores = this.calculateScores(
        observations,
        lastForecasts,
        entries,
      );
      console.log(entryScores);
      this.displayScore(entryScores);
    });
  }

  async getReadings(competition) {
    const observations = await this.weather_data.get_observations(
      competition.locations,
      {
        start: competition.startTime,
        end: competition.endTime,
      },
    );
    const station_observations = {};
    for (let observation of observations) {
      station_observations[observation.station_id] = observation;
    }
    return station_observations;
  }

  calculateScores(weatherReadings, lastForecasts, entries) {
    for (let entry of entries) {
      let currentScore = 0;
      for (let option of entry.expected_observations) {
        const station_id = option.stations;
        const forecast = lastForecasts[station_id];
        if (!forecast) {
          console.error("no forecast found for:", station_id);
          continue;
        }
        const observation = weatherReadings[station_id];
        if (!observation) {
          console.error("no observations found for:", station_id);
          continue;
        }
        Object.keys(option).forEach((key) => {
          if (key == "stations") {
            return;
          }
          console.log(option);
          console.log(key);
          if (option[key] !== null && option[key] !== undefined) {
            const val = option[key];
            console.log(val);
            option[key] = val.toLowerCase();
          }
          const optionScore = this.calculateOptionScore(
            forecast[key],
            observation[key],
            option[key],
          );
          console.log(optionScore);
          console.log(option[key]);
          if (option[key]) {
            const picked = option[key];
            option[key] = {
              score: optionScore,
              val: picked,
              forecast: forecast[key],
              observation: observation[key],
            };
          }
          currentScore += optionScore;
        });
      }

      if (entry.score && entry.score != currentScore) {
        console.error(
          "calculated score does not match oracle response: ",
          entry.score,
          currentScore,
        );
      }
    }
    entries.sort((a, b) => b.score - a.score);
    return entries;
  }

  calculateOptionScore(forecast_val, observation_val, entry_val) {
    if (forecast_val > observation_val) {
      return entry_val == "over" ? 1 : 0;
    } else if (forecast_val == observation_val) {
      return entry_val == "par" ? 2 : 0;
    } else {
      return entry_val == "under" ? 1 : 0;
    }
  }

  clearTable() {
    let $competitionsDataTable = document.getElementById(
      "competitionLeaderboardData",
    );
    let $tbody = $competitionsDataTable.querySelector("tbody");
    if ($tbody) {
      while ($tbody.firstChild) {
        $tbody.removeChild($tbody.firstChild);
      }
    }
  }

  displayScore(entryScores) {
    let $competitionsDataTable = document.getElementById(
      "competitionLeaderboardData",
    );
    let $tbody = $competitionsDataTable.querySelector("tbody");
    if (!$tbody) {
      $tbody = document.createElement("tbody");
      $competitionsDataTable.appendChild($tbody);
    }
    entryScores.forEach((entryScore, index) => {
      let $row = document.createElement("tr");
      $row.classList.add("is-clickable");

      const rank = document.createElement("td");
      rank.textContent = index + 1;
      $row.appendChild(rank);
      if (entryScore["score"] == undefined || entryScore["score"] == null) {
        console.error("no score found for entry:", entryScore["id"]);
        return;
      }

      const cellId = document.createElement("td");
      cellId.textContent = entryScore["id"];
      $row.appendChild(cellId);

      const cellScore = document.createElement("td");
      cellScore.textContent = entryScore["score"];
      $row.appendChild(cellScore);

      $row.addEventListener("click", () => {
        this.handleEntryClick($row, entryScore);
      });

      $tbody.appendChild($row);
      this.showLeaderboard();
    });
  }

  showLeaderboard() {
    let $currentCompetitionCurrent = document.getElementById(
      "competitionLeaderboard",
    );
    $currentCompetitionCurrent.classList.remove("hidden");
  }

  hideLeaderboard() {
    let $currentCompetitionCurrent = document.getElementById(
      "competitionLeaderboard",
    );
    $currentCompetitionCurrent.classList.add("hidden");
  }

  async getEntries(competition) {
    const response = await fetch(
      `${this.oracle_url}/oracle/events/${competition.id}`,
    );
    if (!response.ok) {
      console.error(response);
      throw new Error(
        `Failed to get event entries, status: ${response.status}`,
      );
    }
    let event = await response.json();
    console.log(event);
    return event.entries;
  }

  handleEntryClick($row, entry) {
    const $parentElement = $row.parentElement;
    const $rows = $parentElement.querySelectorAll("tr");
    $rows.forEach(($currentRow) => {
      if ($currentRow != $row) {
        $currentRow.classList.remove("is-selected");
      }
    });
    $row.classList.toggle("is-selected");
    this.showEntryScores(entry);
  }

  showEntryScores(entry) {
    let $entryScoreModal = document.getElementById("entryScore");
    this.clearEntry();

    let $entryValues = document.getElementById("entryValues");
    let $competitionId = document.createElement("h3");
    $competitionId.textContent = `Competition: ${entry.competition_id}`;
    $entryValues.appendChild($competitionId);
    console.log(entry);
    for (let option of entry["expected_observations"]) {
      let $stationDiv = document.createElement("div");
      if (option["stations"]) {
        let $stationHeader = document.createElement("h5");
        $stationHeader.textContent = `Station: ${option.stations}`;
        $stationHeader.classList.add("ml-2");
        $stationDiv.appendChild($stationHeader);
      }
      let $stationList = document.createElement("ul");
      if (option["wind_speed"]) {
        this.buildEntryScorePick(
          $stationList,
          "Wind Speed",
          option["wind_speed"],
        );
      }
      if (option["temp_high"]) {
        this.buildEntryScorePick(
          $stationList,
          "High Temp",
          option["temp_high"],
        );
      }

      if (option["temp_low"]) {
        this.buildEntryScorePick($stationList, "Low Temp", option["temp_low"]);
      }

      $stationDiv.appendChild($stationList);
      $entryValues.appendChild($stationDiv);
    }

    let $totalPts = document.createElement("h6");
    $totalPts.textContent = `Total Points: ${entry.score}`;
    $entryValues.appendChild($totalPts);

    if (!$entryScoreModal.classList.contains("is-active")) {
      $entryScoreModal.classList.add("is-active");
    }
  }

  buildEntryScorePick($stationList, type, option) {
    let $optionListItem = document.createElement("li");
    // forecast val, observation val, val, score
    //$optionListItem.textContent = `Wind Speed ${} ${} ${} ${}`
    $optionListItem.classList.add("ml-4");
    $optionListItem.textContent = `${type}: `;
    let $breakdown = document.createElement("ul");

    let $forecast = document.createElement("li");
    $forecast.classList.add("ml-6");
    $forecast.textContent = `Forecast: ${option["forecast"]}`;
    $breakdown.appendChild($forecast);

    let $observation = document.createElement("li");
    $observation.classList.add("ml-6");
    $observation.textContent = `Observation: ${option["observation"]}`;
    $breakdown.appendChild($observation);

    let $pick = document.createElement("li");
    $pick.classList.add("ml-6");
    $pick.textContent = `Pick: ${option["val"]}`;
    $breakdown.appendChild($pick);

    let $score = document.createElement("li");
    $score.classList.add("ml-6");
    $score.textContent = `Score: ${option["score"]}`;
    $breakdown.appendChild($score);
    $optionListItem.appendChild($breakdown);
    $stationList.appendChild($optionListItem);
  }

  hideEntry() {
    let $entryScoreModal = document.getElementById("entryScore");
    if ($entryScoreModal.classList.contains("is-active")) {
      $entryScoreModal.classList.remove("is-active");
    }
  }

  clearEntry() {
    let $entryValues = document.getElementById("entryValues");
    if ($entryValues) {
      while ($entryValues.firstChild) {
        $entryValues.removeChild($entryValues.firstChild);
      }
    }
  }
}

export { LeaderBoard };
