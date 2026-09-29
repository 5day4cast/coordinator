const assert = require("node:assert/strict");
const test = require("node:test");
const { webcrypto } = require("node:crypto");
const { loadBundle } = require("./bundle.cjs");

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
        networkFee: "50",
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

// The bundle shares the wallet session, isLoggedIn and its other scripts'
// names (AuthorizedClient, openModal) inside one scope; a test hands them in.
function load(page, document, fetch) {
  // The wallet makes entry ids (DlcWallet.newEntryId).
  const wasm = { DlcWallet: { newEntryId: () => "0190b6a0-0000-7000-8000-000000000001" } };
  const session = { wasm, nostrClient: page.nostrClient ?? null, dlcWallet: page.dlcWallet ?? null };
  const isLoggedIn = () => Boolean(page.isLoggedIn?.());
  const window = {};
  const entryForm = loadBundle(["fragments/entry_form/entry_form.js"],
    { ...page, window, document, fetch, crypto: webcrypto, TextEncoder, console, session, isLoggedIn },
    ["submitEntry", "collectPicks", "togglePick", "ticketPriceSats", "loadEntryTerms", "Entry"]);
  assert.deepEqual(Object.keys(window), [], "nothing is put on window");
  return entryForm;
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

// A ticket's price as the coordinator reports it: the form's entry and service fees, and a
// network fee fixed when it was issued.
function price(network_fee_sats = 50, extra = {}) {
  const total = 5250 + network_fee_sats;
  return { entry_fee_sats: 5000, coordinator_fee_sats: 250, network_fee_sats,
    ticket_price_sats: total, amount_sats: total, ...extra };
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
  const entryForm = load({}, document, termsFetch());
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 5000, coordinator_fee_percentage: 5 }), 5250);
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 333, coordinator_fee_percentage: 5 }), 350);
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 1000, coordinator_fee_percentage: 0 }), 1000);
  // Basis points, as the coordinator now sends them: 2.5% of 5000 is 125.
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 5000, coordinator_fee_basis_points: 250 }), 5125);
  // Exact halves round up, matching CoordinatorFee::fee_for (0.5 and 1.5 sats).
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 20, coordinator_fee_basis_points: 250 }), 21);
  assert.equal(entryForm.ticketPriceSats({ entry_fee: 50, coordinator_fee_percentage: 3 }), 52);
});

test("picks come from the checked radios, grouped by station", () => {
  const { document, elements } = entryPage([
    { name: "KPWM_temp_high", value: "over" },
    { name: "KPWM_wind_speed", value: "par" },
    { name: "KBTV_temp_low", value: "under" },
  ]);
  const entryForm = load({}, document, termsFetch());
  assert.deepEqual(JSON.parse(JSON.stringify(entryForm.collectPicks(elements.entryForm))), {
    KPWM: { temp_high: "over", wind_speed: "par" },
    KBTV: { temp_low: "under" },
  });
});

test("choosing a pick again takes it back; choosing another moves it", () => {
  const { document } = entryPage();
  const entryForm = load({ CSS: { escape: (name) => name } }, document, termsFetch());
  const form = {};
  // The browser checks a radio before its click handler runs.
  const row = ["under", "par", "over"].map((value) => element({ name: "KPWM_temp_high", value, form }));
  form.querySelectorAll = (selector) => {
    assert.equal(selector, 'input[name="KPWM_temp_high"]');
    return row;
  };
  const choose = (input) => {
    for (const other of row) other.checked = other === input;
    entryForm.togglePick(input);
  };
  const [under, par] = row;

  choose(par);
  assert.deepEqual(row.map((input) => input.checked), [false, true, false]);
  choose(under);
  assert.deepEqual(row.map((input) => input.checked), [true, false, false]);
  choose(under);
  assert.deepEqual(row.map((input) => input.checked), [false, false, false], "chosen again: no pick");
  choose(under);
  assert.equal(under.checked, true, "and it can be picked once more");
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
        return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc53000n1ticket",
          ...price(), keymeld_session_id: "session", keymeld_registration: {
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
  await sandbox.submitEntry();
  assert.equal(consent.ticket_amount_sats, 5300, "the approved amount includes the coordinator and network fees");
  assert.equal(consent.lightning_address, "thor@lnurl.5day4cast.com");
  assert.equal(consent.max_fee_rate_sat_vb, 5);
  assert.equal(elements.errorMessage.textContent, refusal);
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
  assert.equal(elements.submitEntry.disabled, false);
});

test("a refused ticket shows the coordinator's reason, such as having entered already", async () => {
  const { elements, document } = entryPage();
  const reason = "You've already entered this competition";
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url) {
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        const error = new Error("HTTP error! status: 400");
        error.response = { ok: false, status: 400, json: async () => ({ error: reason }) };
        throw error;
      }
    },
    dlcWallet: { entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }) },
  });
  const sandbox = load(window, document, termsFetch());
  await sandbox.submitEntry();
  assert.equal(elements.errorMessage.textContent, reason);
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
});

test("a signed-out visitor is asked to log in before anything is requested", async () => {
  const { elements, document } = entryPage();
  let opened = null;
  const sandbox = load({ isLoggedIn: () => false, openModal: (modal) => { opened = modal; } }, document,
    async () => assert.fail("nothing may be fetched while signed out"));
  await sandbox.submitEntry();
  assert.equal(opened, elements.loginModal);
});

test("changed terms never tell the user to reload, which would log them out", async () => {
  const { elements, document } = entryPage();
  const window = loggedIn({
    AuthorizedClient: class { async post() { assert.fail("no ticket for changed terms"); } },
    dlcWallet: { entryRegistration: () => assert.fail("no entry key for changed terms") },
  });
  const sandbox = load(window, document, termsFetch({ ...EVENT, coordinator_fee_percentage: 10 }));
  await sandbox.submitEntry();
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
  await sandbox.submitEntry();
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
  await sandbox.submitEntry();
  assert.match(elements.errorMessage.textContent, /up to 2 picks/);
});

// A player whose ticket has an Arkade escrow: the wallet seals their entry key
// to Keymeld, and the page must hand that to the coordinator before it shows
// the invoice, or a ticket paid and never entered could not be refunded.
function registeringPlayer({ refuseRegistration = false } = {}) {
  const { elements, document } = entryPage();
  const requests = [];
  let shownAfter = null;
  const getElementById = document.getElementById;
  document.getElementById = (id) => {
    if (id === "ticketPaymentModal") {
      shownAfter = [...requests];
      throw new Error("the invoice is shown");
    }
    return getElementById(id);
  };
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url, body) {
        requests.push({ url, body });
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        if (url.endsWith("/registration")) {
          if (refuseRegistration) {
            const error = new Error("HTTP error! status: 400");
            error.response = { ok: false, status: 400 };
            throw error;
          }
          return { ok: true, status: 204 };
        }
        return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc53000n1ticket",
          ...price(), keymeld_session_id: "session", keymeld_registration: {
            user_id: "ticket", session_id: "session", payout_policy: "policy" } }) };
      }
    },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: async () => ({
        encrypted_private_key: "sealed", auth_pubkey: "auth",
        context: { user_id: "ticket" }, escrow_policy: { policy: "signed" },
      }),
    },
  });
  const sandbox = load(window, document, termsFetch());
  return { sandbox, elements, requests, shown: () => shownAfter };
}

test("the ticket's Keymeld registration is sent before its invoice is shown", async () => {
  const { sandbox, requests, shown } = registeringPlayer();
  await sandbox.submitEntry();
  const registration = requests.find(({ url }) => url.endsWith("/registration"));
  assert.equal(registration.url,
    `https://coordinator/api/v1/competitions/${COMPETITION}/tickets/ticket/registration`);
  assert.deepEqual(JSON.parse(JSON.stringify(registration.body)), {
    ephemeral_pubkey: "pubkey",
    encrypted_keymeld_private_key: "sealed",
    keymeld_auth_pubkey: "auth",
    keymeld_registration_context: { user_id: "ticket" },
    keymeld_escrow_policy: { policy: "signed" },
  });
  assert.ok(shown()?.includes(registration), "the registration went out before the invoice was shown");
});

test("a refused registration never shows the invoice", async () => {
  const { sandbox, elements, shown } = registeringPlayer({ refuseRegistration: true });
  await sandbox.submitEntry();
  assert.equal(shown(), null, "no invoice to pay");
  assert.ok(!elements.errorMessage.classList.contains("hidden"), "the player is told it failed");
  assert.equal(elements.submitEntry.disabled, false);
});

// A queued competition: the form says so and shows the pool sizes, and the
// wallet checks the entry against the oracle's reference event and key.
const REFERENCE_EVENT = `{"id":"${COMPETITION}","lines":[{"target":"KPWM","metric":"temp_low","lower":-2.5,"upper":-0.0}],"event_announcement":{"expiry":1790259200}}`;

function queuedPage() {
  const page = entryPage();
  Object.assign(page.elements.entryForm.dataset, { kind: "queued", poolMinPlayers: "2", poolMaxPlayers: "25" });
  return page;
}

function queuedFetch({ kind = "queued", pool_rules = { min_players: 2, max_players: 25 } } = {}) {
  const requests = [];
  const fetch = async (url) => {
    requests.push(url);
    if (url.endsWith("/payout-terms")) {
      return { ok: true, json: async () => ({ enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5, arkade: true }) };
    }
    if (url === "https://oracle/oracle/pubkey") {
      return { ok: true, json: async () => ({ key: "A3oracleKeyBase64" }) };
    }
    if (url === `https://oracle/oracle/events/${COMPETITION}`) {
      return { ok: true, text: async () => REFERENCE_EVENT, json: async () => JSON.parse(REFERENCE_EVENT) };
    }
    return { ok: true, json: async () => ({ id: COMPETITION, kind, pool_rules, event_submission: EVENT }) };
  };
  return { fetch, requests };
}

function queuedPlayer(wallet) {
  return loggedIn({
    AuthorizedClient: class {
      async post(url) {
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        // A queued ticket's id is its entry's.
        return { ok: true, json: async () => ({ ticket_id: "0190b6a0-0000-7000-8000-000000000001",
          payment_request: "lnbc53000n1ticket", ...price(), keymeld_session_id: COMPETITION, keymeld_registration: {
            user_id: "0190b6a0-0000-7000-8000-000000000001", session_id: COMPETITION, payout_policy: "policy" } }) };
      }
    },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: () => assert.fail("a queued entry is checked as one"),
      keymeldRegistration: () => assert.fail("escrow must not downgrade"),
      ...wallet,
    },
  });
}

test("a queued entry is checked against the oracle and the form, with no extra step", async () => {
  const { elements, document } = queuedPage();
  const { fetch, requests } = queuedFetch();
  let consent;
  const refusal = "The entry's terms differ from the competition, its oracle event or the ticket";
  const sandbox = load(queuedPlayer({
    keymeldQueuedRegistration: async (entry, assignment, serialized) => {
      assert.equal(entry, "0190b6a0-0000-7000-8000-000000000001");
      assert.equal(JSON.parse(assignment).session_id, COMPETITION);
      consent = JSON.parse(serialized);
      throw refusal;
    },
  }), document, fetch);
  await sandbox.submitEntry();
  assert.ok(requests.includes("https://oracle/oracle/pubkey"), "the key comes from the oracle");
  assert.ok(requests.includes(`https://oracle/oracle/events/${COMPETITION}`));
  assert.deepEqual(Object.keys(consent).sort(), [
    "allow_invoice_fallback", "competition_id", "entry_fee_sats", "expected_relative_locktime_delta",
    "lightning_address", "max_fee_rate_sat_vb", "oracle_pubkey", "pool_rules", "reference_event",
    "release_entry_key_after_payment", "ticket_amount_sats", "ticket_invoice",
  ]);
  assert.equal(consent.competition_id, COMPETITION);
  assert.equal(consent.ticket_amount_sats, 5300);
  assert.equal(consent.entry_fee_sats, 5000);
  assert.deepEqual(consent.pool_rules, { min_players: 2, max_players: 25 });
  assert.equal(consent.oracle_pubkey, "A3oracleKeyBase64");
  assert.equal(consent.lightning_address, "thor@lnurl.5day4cast.com");
  assert.equal(consent.reference_event, REFERENCE_EVENT, "the oracle's text reaches the wallet unchanged");
  assert.ok(consent.reference_event.includes('"upper":-0.0'));
  assert.equal(elements.errorMessage.textContent, refusal);
  assert.equal(elements.submitEntry.disabled, false);
});

test("a queue whose kind or pool sizes differ from the form is refused before any ticket", async () => {
  for (const [label, page, fetchOptions] of [
    ["the API says single", queuedPage(), { kind: null }],
    ["the form says single", entryPage(), {}],
    ["other pool sizes", queuedPage(), { pool_rules: { min_players: 3, max_players: 25 } }],
    ["no pool sizes", queuedPage(), { pool_rules: null }],
  ]) {
    const { elements, document } = page;
    const { fetch } = queuedFetch(fetchOptions);
    const window = loggedIn({
      AuthorizedClient: class { async post() { assert.fail(`no ticket when ${label}`); } },
      dlcWallet: { entryRegistration: () => assert.fail(`no entry key when ${label}`) },
    });
    const sandbox = load(window, document, fetch);
    await sandbox.submitEntry();
    assert.match(elements.errorMessage.textContent, /terms changed/, label);
  }
});

// The ticket's network fee is fixed when it is issued. The wallet is handed the form's price plus
// that fee, and the form shows the ticket's own fee in place of the estimate.
function pricedPlayer(ticketPrice) {
  const { elements, document } = entryPage();
  elements.networkFee = element({ textContent: "50 sats" });
  elements.ticketTotal = element({ textContent: "5,300 sats" });
  let consent = null;
  const refusal = "stop before sealing";
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url) {
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc1ticket",
          ...ticketPrice, keymeld_session_id: "session", keymeld_registration: {
            user_id: "ticket", session_id: "session", payout_policy: "policy" } }) };
      }
    },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: async (_entry, _assignment, serialized) => {
        consent = JSON.parse(serialized);
        throw refusal;
      },
    },
  });
  const sandbox = load(window, document, termsFetch());
  return { sandbox, elements, consent: () => consent, refusal };
}

test("the ticket's own network fee is what the wallet approves and the form shows", async () => {
  const { sandbox, elements, consent, refusal } = pricedPlayer(price(62));
  await sandbox.submitEntry();
  assert.equal(consent().ticket_amount_sats, 5312);
  assert.equal(elements.networkFee.textContent, "62 sats");
  assert.equal(elements.ticketTotal.textContent, "5,312 sats");
  assert.equal(elements.errorMessage.textContent, refusal);
});

test("a ticket priced other than the form showed never reaches the wallet", async () => {
  for (const [label, ticketPrice] of [
    ["another entry fee", price(50, { entry_fee_sats: 5001 })],
    ["another service fee", price(50, { coordinator_fee_sats: 251 })],
    ["a total that is not the sum", price(50, { ticket_price_sats: 5301 })],
    ["an invoice for another amount", price(50, { amount_sats: 5301 })],
    ["no breakdown", { amount_sats: 5300 }],
    ["a negative network fee", price(-1)],
  ]) {
    const { sandbox, elements, consent } = pricedPlayer(ticketPrice);
    await sandbox.submitEntry();
    assert.equal(consent(), null, label);
    assert.match(elements.errorMessage.textContent, /terms changed/, label);
  }
});

test("a network fee more than twice the form's estimate is refused", async () => {
  const within = pricedPlayer(price(100));
  await within.sandbox.submitEntry();
  assert.equal(within.consent().ticket_amount_sats, 5350);

  const { sandbox, elements, consent } = pricedPlayer(price(101));
  await sandbox.submitEntry();
  assert.equal(consent(), null);
  assert.match(elements.errorMessage.textContent, /network fee rose to 101 sats/);
  assert.doesNotMatch(elements.errorMessage.textContent, /reload/i);
});

test("without a network fee estimate no ticket is requested", async () => {
  const { elements, document } = entryPage();
  delete elements.entryForm.dataset.networkFee;
  const window = loggedIn({
    AuthorizedClient: class {
      async post(url) {
        assert.ok(url.endsWith("/api/v1/users/login"), "no ticket without an estimate");
        return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
      }
    },
    dlcWallet: { entryRegistration: () => assert.fail("no entry key without an estimate") },
  });
  const sandbox = load(window, document, termsFetch());
  await sandbox.submitEntry();
  assert.match(elements.errorMessage.textContent, /network fee estimate is unavailable/);
});
