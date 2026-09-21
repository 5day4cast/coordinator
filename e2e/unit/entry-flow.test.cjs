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

// A page holding the entry form as the coordinator renders it for EVENT.
function entryPage() {
  const elements = {
    entryForm: element({
      dataset: {
        competitionId: COMPETITION,
        entryFee: "5000",
        ticketPrice: "5250",
        totalPool: "15000",
        winnerCount: "1",
        maxValues: "1",
      },
      querySelectorAll: () => [{ name: "KPWM_temp_high", value: "over" }],
    }),
    submitEntry: element(),
    errorMessage: element({ classes: ["hidden"] }),
    successMessage: element({ classes: ["hidden"] }),
    entryPayoutApproved: element(),
    entryPayoutTermsText: element({ textContent: "Loading payout terms…" }),
    entryPayoutDestination: element({ textContent: "Log in to see where your winnings are paid." }),
  };
  const document = {
    body: { dataset: { apiBase: "https://coordinator", oracleBase: "https://oracle" } },
    getElementById: (id) => elements[id] ?? null,
    querySelectorAll: () => [],
  };
  return { elements, document };
}

function load(window, document, fetch) {
  const sandbox = { window, document, fetch, crypto: webcrypto, TextEncoder, console,
    lightningPayReq: { decode: () => ({ satoshis: 5250 }) } };
  vm.runInNewContext(readFileSync(path.join(__dirname,
    "../../crates/coordinator/src/templates/pages/entries/entries.js"), "utf8"), sandbox);
  return sandbox;
}

function termsFetch(event = EVENT) {
  return async (url) => {
    if (url.endsWith("/payout-terms")) {
      return { ok: true, json: async () => ({ enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5 }) };
    }
    if (url.startsWith("https://oracle/")) {
      return { ok: true, json: async () => ({ id: COMPETITION, event_announcement: {} }) };
    }
    return { ok: true, json: async () => ({ id: COMPETITION, event_submission: event }) };
  };
}

test("ticket price matches the server's rounding of the coordinator fee", () => {
  const { document } = entryPage();
  const { ticketPriceSats } = load({}, document, termsFetch());
  assert.equal(ticketPriceSats({ entry_fee: 5000, coordinator_fee_percentage: 5 }), 5250);
  assert.equal(ticketPriceSats({ entry_fee: 333, coordinator_fee_percentage: 5 }), 350);
  assert.equal(ticketPriceSats({ entry_fee: 1000, coordinator_fee_percentage: 0 }), 1000);
});

test("entry approves the full ticket price and the profile address, and shows the wallet's own refusal", async () => {
  const { elements, document } = entryPage();
  elements.entryPayoutApproved.checked = true;
  let consent;
  const refusal = "Payout policy differs from the approved entry and ticket";
  const window = {
    isLoggedIn: () => true,
    nostrClient: { isSignerReady: () => true },
    entryPayoutAddress: "thor@lnurl.5day4cast.com",
    entryPayoutAddressLoaded: true,
    entryPayoutTerms: {
      competition: { id: COMPETITION, event_submission: EVENT },
      quote: { enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5 },
      oracle: { event_announcement: {} },
    },
    AuthorizedClient: class { async post() {
      return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc52500n1ticket",
        keymeld_session_id: "session", keymeld_registration: {
          user_id: "ticket", session_id: "session", payout_policy: "policy" } }) };
    } },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: async (_entry, _assignment, serialized) => {
        consent = JSON.parse(serialized);
        // WASM rejects with a plain string, not an Error.
        throw refusal;
      },
    },
  };
  window.dlcWallet.keymeldRegistration = () => assert.fail("escrow must not downgrade");
  const sandbox = load(window, document, termsFetch());
  await sandbox.window.submitEntry();
  assert.equal(consent.ticket_amount_sats, 5250, "the approved amount includes the coordinator fee");
  assert.equal(consent.lightning_address, "thor@lnurl.5day4cast.com");
  assert.equal(elements.errorMessage.textContent, refusal);
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
});

test("a full page load gets payout terms, then the profile address once the user logs in", async () => {
  const { elements, document } = entryPage();
  const window = {
    isLoggedIn: () => false,
    AuthorizedClient: class { async post(url) {
      assert.ok(url.endsWith("/api/v1/users/login"));
      return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
    } },
  };
  const sandbox = load(window, document, termsFetch());
  await sandbox.window.setupEntryPayoutConsent();
  assert.equal(sandbox.window.entryPayoutTerms.competition.id, COMPETITION);
  assert.match(elements.entryPayoutTermsText.textContent, /Maximum Bitcoin fee rate: 5 sat\/vB/);
  assert.equal(elements.entryPayoutDestination.textContent, "Log in to see where your winnings are paid.");
  assert.equal(sandbox.window.entryPayoutAddress, null);

  window.isLoggedIn = () => true;
  window.nostrClient = {};
  elements.entryPayoutApproved.checked = true;
  await sandbox.window.refreshEntryPayoutAddress();
  assert.equal(sandbox.window.entryPayoutAddress, "thor@lnurl.5day4cast.com");
  assert.equal(elements.entryPayoutDestination.textContent, "Automatically to thor@lnurl.5day4cast.com");
  assert.equal(elements.entryPayoutApproved.checked, false, "a new destination needs fresh consent");
});

test("changed terms never tell the user to reload, which would log them out", async () => {
  const { elements, document } = entryPage();
  const sandbox = load({ isLoggedIn: () => false }, document,
    termsFetch({ ...EVENT, coordinator_fee_percentage: 10 }));
  await sandbox.window.setupEntryPayoutConsent();
  assert.equal(sandbox.window.entryPayoutTerms, null);
  assert.match(elements.entryPayoutTermsText.textContent, /terms changed/);
  assert.doesNotMatch(elements.entryPayoutTermsText.textContent, /reload/i);
});

test("a failed profile lookup refuses the entry instead of dropping automatic payouts", async () => {
  const { elements, document } = entryPage();
  elements.entryPayoutApproved.checked = true;
  const requests = [];
  const window = {
    isLoggedIn: () => true,
    nostrClient: { isSignerReady: () => true },
    entryPayoutTerms: {
      competition: { id: COMPETITION, event_submission: EVENT },
      quote: { enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5 },
      oracle: { event_announcement: {} },
    },
    AuthorizedClient: class { async post(url) {
      requests.push(url);
      return { ok: false, status: 503, json: async () => ({}) };
    } },
    dlcWallet: { entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }) },
  };
  const sandbox = load(window, document, termsFetch());
  await sandbox.window.submitEntry();
  assert.match(elements.errorMessage.textContent, /Lightning Address could not be loaded/);
  assert.ok(requests.every((url) => url.endsWith("/api/v1/users/login")), "no ticket may be requested");
});
