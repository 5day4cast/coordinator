// The recovery page: finds a player's entries from their nsec and builds the transactions that
// claim them, with the coordinator gone. The decisions and signatures are the WASM module's
// (coordinator-wasm's RecoveryTool); this script only fetches what it asks for, from Nostr
// relays, an Esplora API and the oracle, and shows the result. The same file is served by the
// coordinator at /recover and shipped in the standalone page (scripts/build-recover-page.sh).

const RECOVER_DEFAULTS = {
  bitcoin: { esplora: "https://mempool.space/api" },
  signet: { esplora: "https://mutinynet.com/api" },
};
const RECOVER_RELAYS = [
  "wss://relay.damus.io",
  "wss://nos.lol",
  "wss://relay.primal.net",
  "wss://relay.nostr.band",
];

function recoverElement(id) {
  return document.getElementById(id);
}

function recoverStatus(text) {
  recoverElement("recover-status").textContent = text;
}

// Every event matching `filters` that `url` holds, until it says it has sent them all.
function recoverFetchRelay(url, filters) {
  return new Promise((resolve) => {
    const events = [];
    const subscription = `recover-${Math.random().toString(36).slice(2, 10)}`;
    let socket = null;
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      try {
        socket.close();
      } catch (_) {
        // Already closed.
      }
      resolve(events);
    };
    const timer = setTimeout(finish, 15000);
    try {
      socket = new WebSocket(url);
    } catch (_) {
      finish();
      return;
    }
    socket.onopen = () => socket.send(JSON.stringify(["REQ", subscription, ...filters]));
    socket.onmessage = (message) => {
      let data;
      try {
        data = JSON.parse(message.data);
      } catch (_) {
        return;
      }
      if (!Array.isArray(data) || data[1] !== subscription) return;
      if (data[0] === "EVENT") events.push(data[2]);
      if (data[0] === "EOSE" || data[0] === "CLOSED") finish();
    };
    socket.onerror = finish;
    socket.onclose = finish;
  });
}

async function recoverFetchRelays(relays, filters) {
  const found = await Promise.all(relays.map((relay) => recoverFetchRelay(relay, filters)));
  return JSON.stringify(found.flat());
}

async function recoverEsplora(base, path) {
  const response = await fetch(base + path);
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`${base}${path}: ${response.status}`);
  return response;
}

// One chain lookup the WASM module asked for, answered in the shape it reads back.
async function recoverAnswer(base, query) {
  if (query.tx) {
    const response = await recoverEsplora(base, `/tx/${query.tx}/status`);
    const status = response && (await response.json());
    return {
      tx: {
        txid: query.tx,
        status: status ? { confirmed_height: status.confirmed ? status.block_height : null } : null,
      },
    };
  }
  const [txid, vout] = query.outspend.split(":");
  const response = await recoverEsplora(base, `/tx/${txid}/outspend/${vout}`);
  const spend = response ? await response.json() : { spent: false };
  const confirmed = spend.spent && spend.status && spend.status.confirmed;
  return {
    outspend: {
      outpoint: query.outspend,
      outspend: {
        spent_by: spend.spent ? spend.txid : null,
        confirmed_height: confirmed ? spend.status.block_height : null,
      },
    },
  };
}

// Run `step` until the chain lookups it needs are all answered.
async function recoverSettle(tool, esplora, step) {
  for (let round = 0; round < 8; round += 1) {
    const result = step();
    if (!result.missing || result.missing.length === 0) return result;
    const answers = await Promise.all(result.missing.map((query) => recoverAnswer(esplora, query)));
    tool.addChain(JSON.stringify(answers));
  }
  throw new Error("the chain lookups did not settle");
}

let recoverWasm = null;

function recoverLoadWasm() {
  if (!recoverWasm) {
    const body = document.body.dataset;
    const glue = new URL(body.wasmGlue, document.baseURI).href;
    const module = new URL(body.wasmModule, document.baseURI).href;
    recoverWasm = import(glue).then(async (wasm) => {
      await wasm.default({ module_or_path: module });
      return wasm;
    });
    recoverWasm.catch(() => {
      recoverWasm = null;
    });
  }
  return recoverWasm;
}

function recoverLines(text) {
  return text
    .split(/[\s,]+/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

async function recoverStart(event) {
  event.preventDefault();
  const nsecField = recoverElement("recover-nsec");
  const network = recoverElement("recover-network").value;
  const esplora = (recoverElement("recover-esplora").value.trim() ||
    (RECOVER_DEFAULTS[network] || {}).esplora || "").replace(/\/+$/, "");
  const oracle = recoverElement("recover-oracle").value.trim().replace(/\/+$/, "");
  const entries = recoverElement("recover-entries");
  entries.replaceChildren();
  try {
    recoverStatus("Loading the recovery module…");
    const wasm = await recoverLoadWasm();
    const tool = new wasm.RecoveryTool(nsecField.value, network);
    // The module holds the key now; the page keeps no copy.
    nsecField.value = "";

    let relays = recoverLines(recoverElement("recover-relays").value);
    const coordinator = recoverElement("recover-coordinator").value.trim();
    if (coordinator) tool.setCoordinator(coordinator);
    const kitFile = recoverElement("recover-kit").files[0];
    if (kitFile) relays = relays.concat(JSON.parse(tool.addKit(await kitFile.text())));
    if (relays.length === 0) relays = RECOVER_RELAYS;
    relays = [...new Set(relays)];

    recoverStatus(`Reading your records from ${relays.length} relays…`);
    tool.addEvents(await recoverFetchRelays(relays, [JSON.parse(tool.playerFilter())]));
    let loaded = JSON.parse(tool.load());
    if (loaded.competition_filter) {
      tool.addEvents(await recoverFetchRelays(relays, [loaded.competition_filter]));
      loaded = JSON.parse(tool.load());
    }

    recoverStatus("Looking for the oracle's results…");
    for (const request of JSON.parse(tool.attestationRequests())) {
      tool.offerAttestationEvents(await recoverFetchRelays(relays, request.filters));
      if (oracle) {
        try {
          const response = await fetch(`${oracle}/oracle/events/${request.event_id}`);
          if (response.ok) tool.offerOracleEvent(request.competition_id, await response.text());
        } catch (_) {
          // The relays may have had it; the report says if not.
        }
      }
    }

    recoverStatus("Following your money on chain…");
    const hash = (await (await recoverEsplora(esplora, "/blocks/tip/hash")).text()).trim();
    const tip = await (await recoverEsplora(esplora, `/block/${hash}`)).json();
    tool.setTip(tip.height, tip.mediantime);
    const inspected = await recoverSettle(tool, esplora, () =>
      JSON.parse(tool.inspect(Date.now() / 1000)),
    );
    recoverStatus(
      inspected.reports.length === 0
        ? "No entries were found for this nsec."
        : `Found ${inspected.reports.length} entries.`,
    );
    for (const warning of inspected.warnings) {
      const note = document.createElement("p");
      note.className = "help is-warning";
      note.textContent = warning;
      entries.append(note);
    }
    for (const { text, report } of inspected.reports) {
      entries.append(recoverEntry(tool, esplora, text, report));
    }
  } catch (error) {
    recoverStatus(`Stopped: ${error.message || error}`);
  }
}

const RECOVER_CLAIMS = ["broadcast_outcome", "broadcast_expiry", "broadcast_split", "claim_win"];

// One entry: its report, and a claim form when there is something to claim now.
function recoverEntry(tool, esplora, text, report) {
  const box = document.createElement("article");
  box.className = "box recover-entry";
  const summary = document.createElement("pre");
  summary.textContent = text;
  box.append(summary);
  if (!report.now.some((action) => RECOVER_CLAIMS.includes(action))) return box;

  const form = document.createElement("form");
  form.className = "recover-claim";
  const field = (label, name, value) => {
    const wrapper = document.createElement("label");
    wrapper.className = "label";
    wrapper.textContent = label;
    const input = document.createElement("input");
    input.className = "input";
    input.name = name;
    input.value = value;
    input.autocomplete = "off";
    wrapper.append(input);
    form.append(wrapper);
    return input;
  };
  const address = field("Your bitcoin address, for the final claim", "address", "");
  const feeRate = field("Fee rate for the final claim (sat/vB)", "fee", "2");
  const preimage = field("Ticket preimage, if the records lack it (hex)", "preimage", "");
  const build = document.createElement("button");
  build.className = "button is-primary";
  build.type = "submit";
  build.textContent = "Build the transactions";
  form.append(build);
  const output = document.createElement("pre");
  output.className = "recover-output";
  box.append(form, output);

  let steps = [];
  const broadcast = document.createElement("button");
  broadcast.className = "button";
  broadcast.type = "button";
  broadcast.textContent = "Broadcast them";
  broadcast.hidden = true;
  box.append(broadcast);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      const claimed = await recoverSettle(tool, esplora, () =>
        JSON.parse(
          tool.claim(report.entry_id, address.value, Number(feeRate.value), preimage.value),
        ),
      );
      const plan = claimed.plan;
      steps = plan.steps;
      const lines = plan.done.map((done) => `Already on chain: ${done}`);
      for (const step of steps) {
        lines.push(`${step.label} ${step.txid}: ${step.bump}`, step.hex);
      }
      if (plan.waiting) lines.push(`Then: ${plan.waiting}`);
      output.textContent = lines.join("\n");
      broadcast.hidden = steps.length === 0;
    } catch (error) {
      output.textContent = `Error: ${error.message || error}`;
    }
  });
  broadcast.addEventListener("click", async () => {
    const lines = [];
    for (const step of steps) {
      // Idempotent: what the chain already has is skipped.
      const known = await recoverEsplora(esplora, `/tx/${step.txid}/status`);
      if (known) {
        lines.push(`${step.label} ${step.txid}: already known`);
        continue;
      }
      const response = await fetch(`${esplora}/tx`, { method: "POST", body: step.hex });
      const answer = await response.text();
      lines.push(`${step.label} ${step.txid}: ${response.ok ? "broadcast" : answer}`);
      if (!response.ok) break;
    }
    output.textContent = `${output.textContent}\n\n${lines.join("\n")}`;
  });
  return box;
}

// Fills the coordinator's recovery key and relays when the coordinator serves this page.
async function recoverPrefill() {
  const url = document.body.dataset.infoUrl;
  if (!url) return;
  try {
    const response = await fetch(url);
    if (!response.ok) return;
    const info = await response.json();
    const coordinator = recoverElement("recover-coordinator");
    if (!coordinator.value) coordinator.value = info.coordinator_pubkey || info.pubkey || "";
    const relays = recoverElement("recover-relays");
    if (!relays.value && Array.isArray(info.relays)) relays.value = info.relays.join("\n");
  } catch (_) {
    // The coordinator may be gone; that is what this page is for.
  }
}

document.addEventListener("DOMContentLoaded", () => {
  const network = document.body.dataset.network;
  if (network) recoverElement("recover-network").value = network;
  const oracle = document.body.dataset.oracle;
  if (oracle) recoverElement("recover-oracle").value = oracle;
  recoverElement("recover-form").addEventListener("submit", recoverStart);
  recoverPrefill();
  // Starts the download while the player types.
  recoverLoadWasm().catch(() => {});
});
