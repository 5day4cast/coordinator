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
          $row.classList.add("is-clickable");
          $row.id = `competition-${competition.id}`;
          if (this.rowExists(competition.id)) {
            return;
          }
          // Exclude non-display properties
          Object.keys(competition).forEach((key) => {
            if (key !== "locations" && key !== "canJoin" && key !== "phase") {
              const cell = document.createElement("td");
              cell.textContent = competition[key];
              $row.appendChild(cell);
            }
          });

          const cell = document.createElement("td");
          if (competition.canJoin) {
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

                if (competition.canJoin) {
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
      fetch(`${this.coordinator_url}/api/v1/competitions`),
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
        //We will have competition exist prior to the event
        const event = eventsMap.get(competition.id);

        const combinedStatus = this.getCombinedStatus(
          competition.state,
          event ? event.status.toLowerCase() : null,
        );
        console.log(competition.event_submission);
        return {
          id: competition.id,
          startTime: competition.event_submission.start_observation_date,
          endTime: competition.event_submission.end_observation_date,
          signingTime: competition.event_submission.signing_date,
          phase: combinedStatus.phase,
          status: combinedStatus.status,
          canJoin: combinedStatus.canJoin,
          entry_fee: competition.event_submission.entry_fee,
          totalPrizePoolAmt:
            competition.event_submission.total_competition_pool,
          totalEntries: competition.total_entries,
          numOfWinners: competition.event_submission.number_of_places_win,
          locations: competition.event_submission.locations,
        };
      })
      .filter(Boolean);
  }

  getCombinedStatus(competitionState, eventStatus) {
    const PHASE = {
      REGISTRATION: "registration",
      SETUP: "setup",
      READY: "ready",
      IN_PROGRESS: "in progress",
      COMPLETING: "completing",
      FINISHED: "finished",
      FAILED: "failed",
    };

    // Early states related to registration
    if (["created"].includes(competitionState.toLowerCase())) {
      return {
        phase: PHASE.REGISTRATION,
        status: "Open for Registration",
        canJoin: true,
      };
    }

    // Competition is being setup/funded
    if (
      [
        "event_created",
        "entries_submitted",
        "escrow_funds_confirmed",
        "entries_collected",
        "contract_created",
        "nonces_collected",
        "aggregate_nonces_generated",
        "partial_signatures_collected",
        "signing_complete",
        "funding_broadcasted",
      ].includes(competitionState.toLowerCase())
    ) {
      return {
        phase: PHASE.SETUP,
        status: "Setting Up Competition",
        canJoin: false,
      };
    }

    if (
      ["funding_confirmed", "funding_settled"].includes(
        competitionState.toLowerCase(),
      ) &&
      eventStatus === "live"
    ) {
      return {
        phase: PHASE.READY,
        status: "Competition Ready - Waiting to Start",
        canJoin: false,
      };
    }

    // Competition is running
    if (
      eventStatus === "running" &&
      ["funding_confirmed", "funding_settled"].includes(
        competitionState.toLowerCase(),
      )
    ) {
      return {
        phase: PHASE.IN_PROGRESS,
        status: "Competition In Progress",
        canJoin: false,
      };
    }

    // Event completed, waiting for attestation
    if (
      eventStatus === "completed" &&
      !["attested", "signed"].includes(competitionState.toLowerCase())
    ) {
      return {
        phase: PHASE.COMPLETING,
        status: "Awaiting Results",
        canJoin: false,
      };
    }

    // Results being processed
    if (
      (eventStatus === "signed" &&
        competitionState.toLowerCase() === "attested") ||
      ["outcome_broadcasted", "delta_broadcasted"].includes(
        competitionState.toLowerCase(),
      )
    ) {
      return {
        phase: PHASE.COMPLETING,
        status: "Competition Completed",
        canJoin: false,
      };
    }

    // Competition completed
    if (competitionState.toLowerCase() === "completed") {
      return {
        phase: PHASE.FINISHED,
        status: "Competition Paid-Out",
        canJoin: false,
      };
    }

    // Failed or cancelled states
    if (["failed", "cancelled"].includes(competitionState.toLowerCase())) {
      return {
        phase: PHASE.FAILED,
        status:
          competitionState === "failed"
            ? "Competition Failed"
            : "Competition Cancelled",
        canJoin: false,
      };
    }

    // Default/unknown state
    return {
      phase: PHASE.FAILED,
      status: "Unknown Status",
      canJoin: false,
    };
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
    if (competition.canJoin) {
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
