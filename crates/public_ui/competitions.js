import { WeatherData } from "./weather_data.js";
import { LeaderBoard } from "./leader_board.js";
import { Entry } from "./entry.js";
import { one_day_ahead } from "./utils.js";

export async function displayCompetitions(apiBase, oracleBase) {
  let $competitionsDataTable = document.getElementById("competitionsDataTable");
  let $tbody = $competitionsDataTable.querySelector("tbody");
  if (!$tbody) {
    $tbody = document.createElement("tbody");
    $competitionsDataTable.appendChild($tbody);
  }
  let comp = new Competitions(
    apiBase,
    oracleBase,
    $competitionsDataTable,
    $tbody,
  );
  await comp.init();
  console.log("initialized competitions");
}

class Competitions {
  constructor(coordinator_url, oracle_url, $competitionsDataTable, $tbody) {
    this.weather_data = new WeatherData(oracle_url);
    this.oracle_url = oracle_url;
    this.coordinator_url = coordinator_url;
    this.currentMaps = {};
    this.$competitionsDataTable = $competitionsDataTable;
    this.$tbody = $tbody;
  }

  rowExists(id) {
    return this.$tbody.querySelector(`tr[id="competition-${id}"]`);
  }

  async refreshData() {
    this.$tbody.innerHTML = "";
    await this.init();
  }

  async init() {
    Promise.all([this.get_stations(), this.get_combined_competitions()])
      .then(([stations, competitions]) => {
        this.stations = stations;
        this.competitions = competitions;
        this.competitions.forEach((competition) => {
          let $row = document.createElement("tr");
          $row.id = `competition-${competition.id}`;
          if (this.rowExists(competition.id)) {
            return;
          }
          // Exclude the "locations" property
          Object.keys(competition).forEach((key) => {
            if (key !== "locations") {
              const cell = document.createElement("td");
              cell.textContent = competition[key];
              $row.appendChild(cell);
            }
          });
          //TODO: change text depending on what competition state we're at
          const cell = document.createElement("td");
          if (competition.status == "live") {
            cell.textContent = "Create Entry";
          } else {
            cell.textContent = "View Competition";
          }
          $row.appendChild(cell);

          $row.addEventListener("click", (event) => {
            this.handleCompetitionClick($row, competition);
            if (event.target.tagName === "TD") {
              if (event.target === event.target.parentNode.lastElementChild) {
                if (!window.taprootWallet) {
                  const notification = document.createElement("div");
                  notification.className = "notification is-info";
                  notification.style.position = "fixed";
                  notification.style.top = "20px";
                  notification.style.right = "20px";
                  notification.style.zIndex = "1000";
                  notification.innerHTML = `
                    <button class="delete"></button>
                    Please log in to create/view an entry
                  `;

                  document.body.appendChild(notification);

                  notification
                    .querySelector(".delete")
                    .addEventListener("click", () => {
                      notification.remove();
                    });

                  setTimeout(() => {
                    notification.remove();
                  }, 2000);

                  const loginModal = document.getElementById("loginModal");
                  loginModal.classList.add("is-active");
                  document.documentElement.classList.add("is-clipped");
                  return;
                }

                if (competition.status == "live") {
                  let entry = new Entry(
                    this.coordinator_url,
                    this.oracle_url,
                    this.stations,
                    competition,
                    () => this.refreshData(),
                  );
                  entry.init().then(() => {
                    entry.showEntry();
                  });
                }
              }
            }
          });
          this.$tbody.appendChild($row);
        });
      })
      .catch((error) => {
        console.error("Error occurred while fetching data:", error);
      });
  }

  async get_combined_competitions() {
    console.log(this.coordinator_url);
    const [competitionsResponse, eventsResponse] = await Promise.all([
      fetch(`${this.coordinator_url}/competitions`),
      fetch(`${this.oracle_url}/oracle/events`),
    ]);

    if (!competitionsResponse.ok) {
      throw new Error(
        `Failed to get competitions, status: ${competitionsResponse.status}`,
      );
    }
    if (!eventsResponse.ok) {
      throw new Error(
        `Failed to get oracle events, status: ${eventsResponse.status}`,
      );
    }

    const competitions = await competitionsResponse.json();
    const events = await eventsResponse.json();

    const eventsMap = new Map(events.map((event) => [event.id, event]));

    return competitions
      .map((competition) => {
        const event = eventsMap.get(competition.id);
        if (!event) {
          console.warn(
            `No matching event found for competition ${competition.id}`,
          );
          return null;
        }

        return {
          id: competition.id,
          startTime: event.observation_date,
          endTime: one_day_ahead(event.observation_date),
          status: event.status.toLowerCase(),
          entry_fee: competition.entry_fee,
          totalPrizePoolAmt: competition.total_competition_pool,
          totalEntries: event.total_entries,
          numOfWinners: competition.number_of_places_win,
          locations: event.locations,
        };
      })
      .filter(Boolean);
  }

  handleCompetitionClick(row, competition) {
    const parentElement = row.parentElement;
    const rows = parentElement.querySelectorAll("tr");
    rows.forEach((currentRow) => {
      if (currentRow != row) {
        currentRow.classList.remove("is-selected");
      }
    });
    row.classList.toggle("is-selected");
    let rowIsSelected = row.classList.contains("is-selected");
    if (competition["status"] == "live") {
      if (this.leader_board) {
        this.leader_board.hideLeaderboard();
      }
    } else {
      this.hideCurrentCompetition();
      this.leader_board = new LeaderBoard(
        this.coordinator_url,
        this.oracle_url,
        competition,
      );
      this.leader_board
        .init()
        .then((result) => {
          console.log("leaderboard displayed");
        })
        .catch((error) => {
          console.error(error);
        });
    }
  }

  showCurrentCompetition(isSelected) {
    let $currentCompetitionCurrent =
      document.getElementById("currentCompetition");
    if (!isSelected) {
      $currentCompetitionCurrent.classList.add("hidden");
      return;
    }
    $currentCompetitionCurrent.classList.remove("hidden");
  }

  hideCurrentCompetition() {
    let $currentCompetitionCurrent =
      document.getElementById("currentCompetition");
    $currentCompetitionCurrent.classList.add("hidden");
  }

  async getCompetitionPoints(station_ids) {
    let competitionPoints = [];
    for (let station_id of station_ids) {
      let station = this.stations[station_id];
      if (station) {
        competitionPoints.push(station);
      }
    }
    return competitionPoints;
  }

  async get_stations() {
    const stations = await this.weather_data.get_stations();
    let stations_mapping = {};
    for (let station of stations) {
      stations_mapping[station.station_id] = station;
    }
    return stations_mapping;
  }
}

export { Competitions };
