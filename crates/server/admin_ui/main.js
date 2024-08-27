import { uuidv7 } from "https://unpkg.com/uuidv7@^1";

const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase);

window.onload = addDefaults;
window.createCompetition = createCompetition;
const station_locations = await get_stations();
load_stations(station_locations);

document
  .getElementById("competition_payload")
  .addEventListener("input", validateCompetition);

function createCompetition($event) {
  console.log("createCompetition");
  let $competitionElement = document.getElementById("competition_payload");
  let competition = JSON.parse($competitionElement.innerText.trim());
  console.log("comeptition", competition);
  const headers = {
    "Content-Type": "application/json",
  };
  $event.target.classList.add("is-loading");
  fetch(`${apiBase}/competitions`, {
    method: "POST",
    headers: headers,
    body: JSON.stringify(competition),
  })
    .then((response) => {
      if (!response.ok) {
        console.error(response);
        $event.target.classList.remove("is-loading");
      } else {
        console.log("competition: ", competition);
        $event.target.classList.remove("is-loading");
      }
    })
    .catch((e) => {
      $event.target.classList.remove("is-loading");
      console.error("Error submitting entry: {}", e);
    });
}

function validateCompetition() {
  let $competitionElement = document.getElementById("competition_payload");
  let competition = $competitionElement.innerText.trim();
  console.log("comeptition", competition);

  try {
    let competition = JSON.parse(competition);
    $competitionElement.classList.remove("invalid");
  } catch (e) {
    console.log(e);
    $competitionElement.classList.add("invalid");
  }
}

function addDefaults() {
  let $competitionElement = document.getElementById("competition_payload");
  let competitionStr = $competitionElement.innerText.trim();
  try {
    let competition = JSON.parse(competitionStr);
    let locations = competition.locations;
    let tomorrow = getTomorrowUTC();
    let signingDate = new Date(tomorrow);
    signingDate.setUTCDate(tomorrow.getUTCDate() + 2);
    signingDate.setUTCHours(0, 0, 0, 0);
    let updatedCompetition = {
      id: uuidv7(),
      signing_date: signingDate.toISOString(),
      observation_date: tomorrow.toISOString(),
      ...competition,
    };
    $competitionElement.innerHTML = `<code>${JSON.stringify(updatedCompetition, null, 2)}</code>`;
  } catch (e) {
    console.error("Error parsing competition:", e.message);
  }
}

function getTomorrowUTC() {
  const now = new Date();
  const tomorrow = new Date(now);
  tomorrow.setUTCDate(now.getUTCDate() + 1);
  tomorrow.setUTCHours(0, 0, 0, 0);
  return tomorrow;
}

async function get_stations() {
  let response = await fetch(`${oracleBase}/stations`);
  if (!response.ok) {
    throw new Error(`Failed to get stations, status: ${response.status}`);
  }
  return response.json();
}

function load_stations(stations) {
  const $tbody = document.getElementById(`stations_container`);

  stations.forEach((station) => {
    let $row = document.createElement("tr");
    $row.id = `station-${station.station_id}`;
    Object.keys(station).forEach((key) => {
      const cell = document.createElement("td");
      cell.textContent = station[key];
      $row.appendChild(cell);
    });
    $tbody.appendChild($row);
  });
}
