const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");

const COMPETITION = "01a0c225-f3c4-71f3-9f62-4b74859cfc25";
const EVENT = {
  entry_fee: 5000,
  coordinator_fee_percentage: 5,
  total_competition_pool: 15000,
  total_allowed_entries: 3,
  number_of_places_win: 1,
};

function element(extra = {}) {
  const classes = new Set(extra.classes || []);
  return {
    textContent: "",
    checked: false,
    disabled: false,
    dataset: {},
    classList: {
      add: (name) => classes.add(name),
      remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
    },
    ...extra,
  };
}

// A page holding the entry form as the coordinator renders it for EVENT,
// with the given picks already checked.
function entryPage(checked = [{ name: "KPWM_temp_high", value: "over" }]) {
  const elements = {
    entryForm: element({
      dataset: {
        competitionId: COMPETITION,
        entryFee: "5000",
        ticketPrice: "5250",
        totalPool: "15000",
        winnerCount: "1",
        maxValues: "2",
      },
      querySelectorAll: (selector) => {
        assert.equal(selector, 'input[type="radio"]:checked');
        return checked;
      },
    }),
    submitEntry: element(),
    errorMessage: element({ classes: ["hidden"] }),
    successMessage: element({ classes: ["hidden"] }),
    loginModal: element(),
  };
  const document = {
    body: { dataset: { apiBase: "https://coordinator", oracleBase: "https://oracle" } },
    getElementById: (id) => elements[id] ?? null,
    addEventListener: () => {},
  };
  return { elements, document };
}

// The bundle shares the wallet session and isLoggedIn between its scripts
// (see shared/wasm.js); a test hands them in with the page's window.
function load(window, document, fetch) {
  const session = { wasm: null, nostrClient: window.nostrClient ?? null, dlcWallet: window.dlcWallet ?? null };
  const isLoggedIn = () => Boolean(window.isLoggedIn?.());
  const sandbox = { window, document, fetch, crypto: webcrypto, TextEncoder, console, session, isLoggedIn };
  vm.runInNewContext(readFileSync(path.join(__dirname,
    "../../crates/coordinator/src/templates/fragments/entry_form/entry_form.js"), "utf8"), sandbox);
  return sandbox;
}

function termsFetch(event = EVENT, quote = {}) {
  return async (url) => {
    if (url.endsWith("/payout-terms")) {
      return { ok: true, json: async () => ({ enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5, ...quote }) };
    }
    if (url.startsWith("https://oracle/")) {
      return { ok: true, json: async () => ({ id: COMPETITION, event_announcement: {} }) };
    }
    return { ok: true, json: async () => ({ id: COMPETITION, event_submission: event }) };
  };
}

function loggedIn(extra = {}) {
  return {
    isLoggedIn: () => true,
    nostrClient: { isSignerReady: () => true },
    ...extra,
  };
}

test("ticket price matches the server's rounding of the coordinator fee", () => {
  const { document } = entryPage();
  const { window } = load({}, document, termsFetch());
  assert.equal(window.ticketPriceSats({ entry_fee: 5000, coordinator_fee_percentage: 5 }), 5250);
  assert.equal(window.ticketPriceSats({ entry_fee: 333, coordinator_fee_percentage: 5 }), 350);
  assert.equal(window.ticketPriceSats({ entry_fee: 1000, coordinator_fee_percentage: 0 }), 1000);
});

test("picks come from the checked radios, grouped by station", () => {
  const { document, elements } = entryPage([
    { name: "KPWM_temp_high", value: "over" },
    { name: "KPWM_wind_speed", value: "par" },
    { name: "KBTV_temp_low", value: "under" },
  ]);
  const { window } = load({}, document, termsFetch());
  assert.deepEqual(JSON.parse(JSON.stringify(window.collectPicks(elements.entryForm))), {
    KPWM: { temp_high: "over", wind_speed: "par" },
    KBTV: { temp_low: "under" },
  });
});

test("entering is the consent: the ticket carries the full price and the account's address", async () => {
  const { elements, document } = entryPage();
  let consent;
  const refusal = "Payout policy differs from the approved entry and ticket";
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url) {
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc52500n1ticket",
          keymeld_session_id: "session", keymeld_registration: {
            user_id: "ticket", session_id: "session", payout_policy: "policy" } }) };
      }
    },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: async (_entry, _assignment, serialized) => {
        consent = JSON.parse(serialized);
        // WASM rejects with a plain string, not an Error.
        throw refusal;
      },
      keymeldRegistration: () => assert.fail("escrow must not downgrade"),
    },
  });
  const sandbox = load(window, document, termsFetch());
  await sandbox.window.submitEntry();
  assert.equal(consent.ticket_amount_sats, 5250, "the approved amount includes the coordinator fee");
  assert.equal(consent.lightning_address, "thor@lnurl.5day4cast.com");
  assert.equal(consent.max_fee_rate_sat_vb, 5);
  assert.equal(elements.errorMessage.textContent, refusal);
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
  assert.equal(elements.submitEntry.disabled, false);
});

test("a signed-out visitor is asked to log in before anything is requested", async () => {
  const { elements, document } = entryPage();
  let opened = null;
  const sandbox = load({ isLoggedIn: () => false, openModal: (modal) => { opened = modal; } }, document,
    async () => assert.fail("nothing may be fetched while signed out"));
  await sandbox.window.submitEntry();
  assert.equal(opened, elements.loginModal);
});

test("changed terms never tell the user to reload, which would log them out", async () => {
  const { elements, document } = entryPage();
  const window = loggedIn({
    AuthorizedClient: class { async post() { assert.fail("no ticket for changed terms"); } },
    dlcWallet: { entryRegistration: () => assert.fail("no entry key for changed terms") },
  });
  const sandbox = load(window, document, termsFetch({ ...EVENT, coordinator_fee_percentage: 10 }));
  await sandbox.window.submitEntry();
  assert.match(elements.errorMessage.textContent, /terms changed/);
  assert.doesNotMatch(elements.errorMessage.textContent, /reload/i);
});

test("a failed address lookup refuses the entry instead of dropping automatic payouts", async () => {
  const { elements, document } = entryPage();
  const requests = [];
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url) {
        requests.push(url);
        const error = new Error("HTTP error! status: 503");
        error.response = { ok: false, status: 503 };
        throw error;
      }
    },
    dlcWallet: { entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }) },
  });
  const sandbox = load(window, document, termsFetch());
  await sandbox.window.submitEntry();
  assert.match(elements.errorMessage.textContent, /Lightning Address could not be loaded/);
  assert.ok(requests.every((url) => url.endsWith("/api/v1/users/login")), "no ticket may be requested");
});

test("too many picks are refused before any payment", async () => {
  const { elements, document } = entryPage([
    { name: "KPWM_temp_high", value: "over" },
    { name: "KPWM_temp_low", value: "par" },
    { name: "KPWM_wind_speed", value: "under" },
  ]);
  const sandbox = load(loggedIn({ dlcWallet: {} }), document, async () => assert.fail("nothing fetched"));
  await sandbox.window.submitEntry();
  assert.match(elements.errorMessage.textContent, /up to 2 picks/);
});
