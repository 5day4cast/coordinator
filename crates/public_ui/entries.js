import { one_day_ahead } from "./utils.js";
import { AuthorizedClient } from "./authorized_client.js";

export async function displayEntries(apiBase, oracleBase) {
  let $entriesDataTable = document.getElementById("entriesDataTable");
  let $tbody = $entriesDataTable.querySelector("tbody");
  if (!$tbody) {
    $tbody = document.createElement("tbody");
    $entriesDataTable.appendChild($tbody);
  }
  let comp = new Entries(apiBase, oracleBase, $entriesDataTable, $tbody);
  await comp.init();
  console.log("initialized entries");
}

class Entries {
  constructor(coordinator_url, oracle_url, $tbody) {
    this.coordinator_url = coordinator_url;
    this.oracle_url = oracle_url;
    this.client = new AuthorizedClient(window.nostrClient, coordinator_url);
    this.entries = [];
    this.$tbody = $tbody;
  }

  rowExists(id) {
    return this.$tbody.querySelector(`tr[id="entry-${id}"]`);
  }

  async init() {
    Promise.all([this.get_events(), this.get_user_entries()])
      .then(([events, entries]) => {
        console.log(events);
        console.log(entries);
        this.events = events;
        this.entries = entries;
        entries.forEach((entry) => {
          console.log(entry);
          let $row = document.createElement("tr");
          $row.id = `entry-${entry.id}`;
          $row.classList.add("is-clickable");
          if (this.rowExists(entry.id)) {
            return;
          }
          let event = events.find((val) => val.id === entry.event_id);
          console.log(event);

          const event_id = document.createElement("td");
          event_id.textContent = event.id;
          $row.appendChild(event_id);

          const event_start = document.createElement("td");
          event_start.textContent = event["start_observation_date"];
          $row.appendChild(event_start);

          const event_end = document.createElement("td");
          event_end.textContent = event["end_observation_date"];
          $row.appendChild(event_end);

          const status = document.createElement("td");
          status.textContent = event["status"].toLowerCase();
          $row.appendChild(status);

          const entry_id = document.createElement("td");
          entry_id.textContent = entry.id;
          $row.appendChild(entry_id);

          this.$tbody.appendChild($row);
        });
      })
      .catch((error) => {
        console.error("Error occurred while fetching data:", error);
      });
  }

  async get_events() {
    // Oracle endpoints don't need authentication
    const response = await fetch(`${this.oracle_url}/oracle/events`);
    if (!response.ok) {
      console.error(response);
      throw new Error(`Failed to get competitions, status: ${response.status}`);
    }
    let oracle_events = await response.json();
    console.log("known events: ", oracle_events);
    return oracle_events;
  }

  async get_user_entries() {
    const response = await this.client.get(
      `${this.coordinator_url}/api/v1/entries`,
    );

    if (!response.ok) {
      console.error(response);
      throw new Error(`Failed to get entries, status: ${response.status}`);
    }
    let user_entries = await response.json();
    return user_entries;
  }
}

export { Entries };
