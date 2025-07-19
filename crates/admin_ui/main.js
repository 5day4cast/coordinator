import { uuidv7 } from "https://unpkg.com/uuidv7@^1";

const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase);

window.createCompetition = createCompetition;
window.refreshBalance = refreshBalance;
window.getNewAddress = getNewAddress;
window.refreshOutputs = refreshOutputs;
window.sendBitcoin = sendBitcoin;
window.toggleJson = toggleJson;
window.refreshFeeEstimates = refreshFeeEstimates;

window.onload = async function () {
  addDefaults();
  setupTabNavigation();
  const station_locations = await get_stations();
  load_stations(station_locations);

  document
    .getElementById("competition_payload")
    .addEventListener("input", validateCompetition);

  if (
    document.getElementById("wallet-section").classList.contains("is-active")
  ) {
    await refreshBalance();
    await refreshOutputs();
    await refreshFeeEstimates();
  }
};

function setupTabNavigation() {
  const tabs = document.querySelectorAll(".tabs li");
  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      // Remove active class from all tabs
      tabs.forEach((t) => t.classList.remove("is-active"));
      // Add active class to clicked tab
      tab.classList.add("is-active");

      // Hide all content sections
      const contents = document.querySelectorAll(".tabs-content > div");
      contents.forEach((content) => content.classList.remove("is-active"));

      // Show the selected content section
      const targetId = tab.dataset.target;
      document.getElementById(targetId).classList.add("is-active");

      // If switching to wallet tab, refresh the data
      if (targetId === "wallet-section") {
        refreshBalance();
        refreshOutputs();
      }
    });
  });
}

function createCompetition($event) {
  console.log("createCompetition");
  let $competitionElement = document.getElementById("competition_payload");
  let competition = JSON.parse($competitionElement.innerText.trim());
  competition.total_competition_pool =
    competition.entry_fee * competition.total_allowed_entries;
  console.log("competition", competition);

  const headers = {
    "Content-Type": "application/json",
  };

  $event.target.classList.add("is-loading");

  fetch(
    `${apiBase}/api/v1/competitions`,
    getRequestOptions({
      method: "POST",
      headers: headers,
      body: JSON.stringify(competition),
    }),
  )
    .then((response) => {
      if (!response.ok) {
        console.error(response);
      } else {
        console.log("competition: ", competition);
      }
      $event.target.classList.remove("is-loading");
    })
    .catch((e) => {
      $event.target.classList.remove("is-loading");
      console.error("Error submitting entry: {}", e);
    });
}

function validateCompetition() {
  let $competitionElement = document.getElementById("competition_payload");
  let competition = $competitionElement.innerText.trim();
  try {
    JSON.parse(competition);
    $competitionElement.classList.remove("invalid");
  } catch (e) {
    console.log(`Failed to parse competition {e}`);
    $competitionElement.classList.add("invalid");
  }
}

function addDefaults() {
  let $competitionElement = document.getElementById("competition_payload");
  let competitionStr = $competitionElement.innerText.trim();
  try {
    let competition = JSON.parse(competitionStr);
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
  const $tbody = document
    .getElementById(`stations_container`)
    .querySelector("tbody");
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

async function refreshBalance() {
  try {
    const response = await fetch(
      `${apiBase}/api/v1/wallet/balance`,
      getRequestOptions(),
    );
    if (!response.ok) throw new Error(`HTTP error! status: ${response.status}`);
    const data = await response.json();

    document.getElementById("confirmed-balance").textContent = data.confirmed;
    document.getElementById("unconfirmed-balance").textContent =
      data.unconfirmed;
  } catch (error) {
    console.error("Error fetching balance:", error);
    alert("Failed to fetch balance");
  }
}

async function getNewAddress() {
  try {
    const response = await fetch(
      `${apiBase}/api/v1/wallet/address`,
      getRequestOptions(),
    );
    if (!response.ok) throw new Error(`HTTP error! status: ${response.status}`);
    const data = await response.json();

    document.getElementById("current-address").textContent = data.address;
  } catch (error) {
    console.error("Error getting new address:", error);
    alert("Failed to get new address");
  }
}

async function refreshFeeEstimates() {
  try {
    const response = await fetch(
      `${apiBase}/api/v1/wallet/estimated_fees`,
      getRequestOptions(),
    );
    if (!response.ok) throw new Error(`HTTP error! status: ${response.status}`);
    const data = await response.json();

    const tbody = document.getElementById("fee-estimates-table");
    tbody.innerHTML = "";

    // Sort by confirmation target (number of blocks)
    const sortedEstimates = Object.entries(data.fee_estimates).sort(
      ([blocksA, _a], [blocksB, _b]) => parseInt(blocksA) - parseInt(blocksB),
    );

    sortedEstimates.forEach(([blocks, feeRate]) => {
      const row = document.createElement("tr");
      row.innerHTML = `
        <td>${blocks}</td>
        <td>${feeRate.toFixed(1)}</td>
      `;
      tbody.appendChild(row);
    });
  } catch (error) {
    console.error("Error fetching fee estimates:", error);
    alert("Failed to fetch fee estimates");
  }
}

async function refreshOutputs() {
  try {
    const response = await fetch(
      `${apiBase}/api/v1/wallet/outputs`,
      getRequestOptions(),
    );
    if (!response.ok) throw new Error(`HTTP error! status: ${response.status}`);
    const data = await response.json();

    const tbody = document.getElementById("outputs-table");
    tbody.innerHTML = "";

    data.outputs.forEach((output) => {
      const row = document.createElement("tr");
      row.innerHTML = `
                <td><code>${output.outpoint.split(":")[0]}</code></td>
                <td>${output.txout.value}</td>
                <td><code>${output.txout.script_pubkey || "-"}</code></td>
                <td>${output.is_spent ? "Spent" : "Unspent"}</td>
                <td>
                  <button onclick="toggleJson(this)" class="json-btn">View JSON</button>
                  <pre class="json-data" style="display:none">${JSON.stringify(output, null, 2)}</pre>
                </td>
            `;
      tbody.appendChild(row);
    });
  } catch (error) {
    console.error("Error fetching outputs:", error);
    alert("Failed to fetch outputs");
  }
}

function toggleJson(button) {
  const jsonData = button.nextElementSibling;
  if (jsonData.style.display === "none") {
    jsonData.style.display = "block";
    button.textContent = "Hide JSON";
  } else {
    jsonData.style.display = "none";
    button.textContent = "View JSON";
  }
}

function getRequestOptions(options = {}) {
  if (!apiBase.includes("localhost")) {
    return { ...options, credentials: "include" };
  }
  return options;
}

async function sendBitcoin() {
  const address = document.getElementById("send-address").value;
  const amount = parseInt(document.getElementById("send-amount").value);
  const maxFee = parseInt(document.getElementById("send-fee").value);

  if (!address) {
    alert("Please enter a destination address");
    return;
  }

  const payload = {
    address_to: address,
    amount: amount || undefined,
    max_fee: maxFee || undefined,
  };

  try {
    const response = await fetch(
      `${apiBase}/api/v1/wallet/send`,
      getRequestOptions({
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify(payload),
      }),
    );

    if (!response.ok) throw new Error(`HTTP error! status: ${response.status}`);
    const data = await response.json();

    const resultDiv = document.getElementById("send-result");
    const resultContent = document.getElementById("send-result-content");
    resultContent.textContent = JSON.stringify(data, null, 2);
    resultDiv.classList.remove("is-hidden");

    await refreshBalance();
    await refreshOutputs();
  } catch (error) {
    console.error("Error sending bitcoin:", error);
    alert("Failed to send bitcoin: " + error);
  }
}
