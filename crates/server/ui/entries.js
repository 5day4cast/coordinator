import { get_xonly_pubkey, one_day_ahead } from "./utils.js";

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
          if (this.rowExists(entry.id)) {
            return;
          }
          let event = events.find((val) => val.id === entry.event_id);
          console.log(event);

          const event_id = document.createElement("td");
          event_id.textContent = event.id;
          $row.appendChild(event_id);

          const event_start = document.createElement("td");
          event_start.textContent = event["observation_date"];
          $row.appendChild(event_start);

          const event_end = document.createElement("td");
          event_end.textContent = one_day_ahead(event["observation_date"]);
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
    //TODO: fix the filter for event_ids
    let response = await fetch(`${this.oracle_url}/oracle/events`);
    if (!response.ok) {
      console.error(response);
      throw new Error(`Failed to get competitions, status: ${response.status}`);
    }
    let oracle_events = await response.json();
    console.log("known events: ", oracle_events);
    return oracle_events;
  }

  async get_user_entries() {
    if (!window.nostr) {
      this.showError("user needs to login before submitting");
      return;
    }
    let xonly_pubkey_hex = await window.nostr.getPublicKey();
    //TODO: fix the filter for event_ids
    let search_query = {
      pubkey: xonly_pubkey_hex,
    };
    let query_hash = await hash_object(search_query);
    console.log(query_hash);

    const signature = await window.nostr.signSchnorr(query_hash);

    let response = await fetch(
      `${this.coordinator_url}/entries?pubkey=${pubkey}&signature=${signature}`,
    );
    if (!response.ok) {
      console.error(response);
      throw new Error(`Failed to get competitions, status: ${response.status}`);
    }
    let user_entries = await response.json();
    return user_entries;
  }
}

export { Entries };
