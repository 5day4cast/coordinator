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
  const listeners = {};
  return {
    textContent: "",
    checked: false,
    disabled: false,
    dataset: {},
    attributes: {},
    classList: {
      add: (name) => classes.add(name),
      remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
      toggle: (name, on) => (on ? classes.add(name) : classes.delete(name)),
    },
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
    append() {},
    replaceChildren(...children) { this.children = children; },
    addEventListener: (type, listener) => (listeners[type] ??= new Set()).add(listener),
    removeEventListener: (type, listener) => listeners[type]?.delete(listener),
    dispatch: (type, event = {}) => [...(listeners[type] ?? [])].forEach((listener) => listener(event)),
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
        maxValues: "1",
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
  // The wallet makes entry ids (DlcWallet.newEntryId), a new one each time.
  let made = 0;
  const wasm = { DlcWallet: { newEntryId: () => `0190b6a0-0000-7000-8000-${String(++made).padStart(12, "0")}` } };
  const session = { wasm, nostrClient: page.nostrClient ?? null, dlcWallet: page.dlcWallet ?? null };
  const isLoggedIn = () => Boolean(page.isLoggedIn?.());
  // htmx is the page's own, on window; nothing else is.
  const window = page.htmx ? { htmx: page.htmx } : {};
  const entryForm = loadBundle(["fragments/entry_form/entry_form.js"],
    { ...page, window, document, fetch, crypto: webcrypto, TextEncoder, console, session, isLoggedIn },
    ["submitEntry", "collectPicks", "togglePick", "unpickWithSpace", "ticketPriceSats", "loadEntryTerms", "Entry", "picksLeft", "labelPay"]);
  assert.deepEqual(Object.keys(window), page.htmx ? ["htmx"] : [], "nothing is put on window");
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

test("Space takes back a chosen pick, which browsers don't click, and leaves others to the browser", () => {
  const { document } = entryPage();
  const entryForm = load({ CSS: { escape: (name) => name } }, document, termsFetch());
  const form = { querySelectorAll: () => [pick] };
  const pick = element({ name: "KPWM_temp_high", value: "par", form, matches: (selector) => selector === ".pick-option input[type=radio]" });
  const press = (key) => {
    let prevented = false;
    entryForm.unpickWithSpace({ key, target: pick, preventDefault: () => { prevented = true; } });
    return prevented;
  };

  // Not chosen yet: the browser checks it and clicks, as for any radio.
  assert.equal(press(" "), false);
  pick.checked = true;
  entryForm.togglePick(pick);
  assert.equal(press("Enter"), false, "only Space");
  // Chosen: Space clears it, and stops the key so its release can't check it again.
  assert.equal(press(" "), true);
  assert.equal(pick.checked, false);
  assert.equal(pick.dataset.picked, undefined);
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
  let walletLoads = 0;
  const sandbox = load({
    isLoggedIn: () => false,
    openModal: (modal) => { opened = modal; },
    // Pay is what loads the wallet for a signed-out visitor, not showing the form.
    loadWallet: () => { walletLoads += 1; },
  }, document, async () => assert.fail("nothing may be fetched while signed out"));
  assert.equal(walletLoads, 0);
  await sandbox.submitEntry();
  assert.equal(opened, elements.loginModal);
  assert.equal(walletLoads, 1);
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
  assert.match(elements.errorMessage.textContent, /Make exactly 1 pick/);
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
  assert.match(elements.errorMessage.textContent, /entry fee rose to 5,351 sats/);
  assert.doesNotMatch(elements.errorMessage.textContent, /network fee/);
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
  assert.match(elements.errorMessage.textContent, /entry fee is unavailable/);
  assert.doesNotMatch(elements.errorMessage.textContent, /network fee/);
});

// Pay with nothing picked says so above the picks and takes the player to the first one,
// before anything is asked of the wallet or the server.
test("paying with no picks says to make one and focuses the first pick", async () => {
  const { elements, document } = entryPage([]);
  let focused = null;
  const first = element({ focus() { focused = this; } });
  elements.entryForm.querySelector = (selector) => {
    assert.equal(selector, ".pick-option input[type=radio]:not(:disabled)");
    return first;
  };
  elements.picksMessage = element({ classes: ["hidden"] });
  const sandbox = load({ isLoggedIn: () => false, openModal: () => assert.fail("no log-in for no picks") },
    document, async () => assert.fail("nothing fetched"));
  await sandbox.submitEntry();
  assert.equal(elements.picksMessage.textContent, "Make exactly 1 pick before paying.");
  assert.ok(!elements.picksMessage.classList.contains("hidden"));
  assert.equal(focused, first);
  assert.equal(elements.submitEntry.disabled, false);
  assert.ok(!elements.submitEntry.classList.contains("is-loading"));
});

test("with the picks still loading, the message shows under Pay", async () => {
  const { elements, document } = entryPage([]);
  elements.entryForm.querySelector = () => null;
  const sandbox = load({}, document, async () => assert.fail("nothing fetched"));
  await sandbox.submitEntry();
  assert.equal(elements.errorMessage.textContent, "Make exactly 1 pick before paying.");
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
});

// A player who gets as far as the invoice: the payment dialog and its status poller, with
// the dialog helpers (modal_utils.js) reduced to what they do to the dialog.
function payingPlayer({ ticket = () => ({ ok: true, json: async () => ({ ticket_id: "ticket",
  payment_request: "lnbc53000n1ticket", ...price(), keymeld_session_id: "session",
  keymeld_registration: { user_id: "ticket", session_id: "session", payout_policy: "policy" } }) }),
  entry = () => ({ ok: true, json: async () => ({ id: "entry" }) }), sessionStorage,
  unpaid = null, listed = [], timers = null, paid = null } = {}) {
  const { elements, document } = entryPage();
  // The notice the form shows for the signed-in player's unpaid ticket, when they hold one.
  if (unpaid) elements.entryUnpaid = element({ dataset: { ticketId: unpaid } });
  // The notice for a ticket they paid for and never entered.
  if (paid) elements.entryPaid = element({ dataset: { ...paid } });
  if (timers) elements.ticketPaymentExpiry = element();
  Object.assign(elements, {
    ticketPaymentModal: element({ querySelector: () => null }),
    copyFeedback: element(),
    ticketPaymentError: element({ classes: ["is-hidden"] }),
    qrContainer: element(),
    walletLinkLightning: element(),
    walletLinkZeus: element(),
    walletLinkCashApp: element(),
    paymentStatus: element(),
    ticketPaymentAmount: element(),
  });
  const modal = elements.ticketPaymentModal;
  const documentListeners = {};
  const polled = [];
  Object.assign(document, {
    createElement: () => element({
      replaceWith() {},
    }),
    querySelectorAll: () => [],
    addEventListener: (type, listener) => (documentListeners[type] ??= new Set()).add(listener),
    removeEventListener: (type, listener) => documentListeners[type]?.delete(listener),
  });
  elements.paymentStatus.replaceWith = (status) => {
    polled.push(status.attributes["hx-get"]);
    elements.paymentStatus = Object.assign(status, { replaceWith: elements.paymentStatus.replaceWith });
  };
  const requests = [];
  const page = loggedIn({
    sessionStorage,
    htmx: { process: () => {} },
    openModal: (dialog) => dialog.classList.add("is-active"),
    // As closeModal does: hide it, then tell whoever opened it.
    closeModal: (dialog) => {
      if (!dialog.classList.contains("is-active")) return;
      dialog.classList.remove("is-active");
      dialog.dispatch("fw:modal-closed");
    },
    setTimeout: () => 0,
    clearTimeout: () => {},
    ...(timers ?? {}),
    AuthorizedClient: class {
      async get(url) {
        requests.push({ url, method: "GET" });
        return { ok: true, json: async () => listed };
      }
      async post(url, body) {
        requests.push({ url, body });
        if (url.endsWith("/api/v1/users/login")) {
          return { ok: true, json: async () => ({ lightning_address: "thor@lnurl.5day4cast.com" }) };
        }
        if (url.endsWith("/registration")) return { ok: true, status: 204 };
        const response = url.endsWith("/api/v1/entries") ? await entry(body) : await ticket(body);
        if (!response.ok) {
          const error = new Error(`HTTP error! status: ${response.status}`);
          error.response = response;
          throw error;
        }
        return response;
      }
    },
    dlcWallet: {
      entryRegistration: () => ({ ephemeral_pubkey: "pubkey", payout_hash: "hash" }),
      keymeldPayoutRegistration: async () => ({
        encrypted_private_key: "sealed", auth_pubkey: "auth", context: { user_id: "ticket" },
      }),
      invoiceQr: (invoice) => `data:image/svg+xml,${invoice}`,
    },
  });
  const sandbox = load(page, document, termsFetch());
  const settle = () => new Promise((resolve) => setImmediate(resolve));
  // Clicks Pay and returns once the invoice is shown or the flow has ended; `done` is the
  // click's flow, which waits for the payment.
  const pay = async () => {
    let ended = false;
    const done = sandbox.submitEntry().finally(() => { ended = true; });
    for (let i = 0; i < 50 && !ended && !modal.classList.contains("is-active"); i++) await settle();
    return { done };
  };
  const tickets = () => requests.filter(({ url }) => url.endsWith("/ticket"));
  const entries = () => requests.filter(({ url }) => url.endsWith("/api/v1/entries"));
  const announce = (type, detail = {}) => [...(documentListeners[type] ?? [])].forEach((listener) => listener({ detail: { ticket_id: "ticket", ...detail } }));
  return { sandbox, elements, modal, pay, settle, tickets, entries, polled, announce, requests };
}

test("the payment dialog shows one all-in amount and no fee wording", async () => {
  const { elements, modal, pay } = payingPlayer();
  await pay();
  assert.ok(modal.classList.contains("is-active"));
  assert.equal(elements.ticketPaymentAmount.textContent,
    "Pay 5,300 sats by Lightning to enter this competition.");
  assert.doesNotMatch(elements.ticketPaymentAmount.textContent, /fee/);
});

test("closing the payment dialog gives Pay back; Pay reopens the same invoice, and a payment made meanwhile enters", async () => {
  const { sandbox, elements, modal, pay, settle, tickets, entries, polled, announce } = payingPlayer();
  const { done: first } = await pay();
  assert.ok(modal.classList.contains("is-active"), "the invoice is shown");
  assert.ok(elements.submitEntry.classList.contains("is-loading"));
  const invoice = elements.walletLinkLightning.href;
  assert.equal(invoice, "lightning:lnbc53000n1ticket");

  // The backdrop, Esc or the close button: all closeModal.
  closeDialog(modal);
  assert.ok(!modal.classList.contains("is-active"));
  assert.equal(elements.submitEntry.disabled, false, "Pay works again");
  assert.ok(!elements.submitEntry.classList.contains("is-loading"), "no spinner");
  assert.equal(polled.length, 1, "the payment is still watched");

  // Pay again: the same ticket and invoice, no second ticket.
  await sandbox.submitEntry();
  assert.ok(modal.classList.contains("is-active"), "the dialog opens again");
  assert.equal(elements.walletLinkLightning.href, invoice);
  assert.equal(tickets().length, 1);
  assert.equal(polled.length, 1, "the same poller");

  // Closed again, then paid from the wallet: the entry goes through all the same.
  closeDialog(modal);
  assert.equal(elements.submitEntry.disabled, false);
  announce("fw:ticket-paid");
  await first;
  assert.equal(entries().length, 1);
  assert.equal(entries()[0].body.ticket_id, "ticket");
  assert.equal(elements.submitEntry.textContent, "Entered");
  assert.ok(!elements.successMessage.classList.contains("hidden"));
  assert.equal(tickets().length, 1);
});

function closeDialog(modal) {
  modal.classList.remove("is-active");
  modal.dispatch("fw:modal-closed");
}

test("another competition cannot replace an unpaid invoice or its poller", async () => {
  const { sandbox, elements, modal, pay, tickets, polled, announce } = payingPlayer();
  const { done } = await pay();
  closeDialog(modal);
  const invoice = elements.walletLinkLightning.href;
  elements.entryForm.dataset.competitionId = "another-competition";
  await sandbox.submitEntry();
  assert.equal(tickets().length, 1);
  assert.equal(polled.length, 1);
  assert.equal(elements.walletLinkLightning.href, invoice);
  assert.equal(modal.dataset.ticketId, "ticket");
  assert.match(elements.errorMessage.textContent, /pending entry in the other competition/);
  announce("fw:ticket-failed", { message: "expired" });
  await done;
});

test("a different ticket's completion cannot finish the active payment", async () => {
  const { elements, modal, pay, settle, entries, announce } = payingPlayer();
  const { done } = await pay();
  const poller = elements.paymentStatus;
  announce("fw:ticket-paid", { ticket_id: "older-ticket" });
  announce("fw:ticket-failed", { ticket_id: "older-ticket", message: "expired" });
  await settle();
  assert.equal(entries().length, 0);
  assert.equal(elements.paymentStatus, poller);
  assert.ok(modal.classList.contains("is-active"));
  announce("fw:ticket-paid");
  await done;
  assert.equal(entries().length, 1);
});

function replaceEntryForm(elements, competitionId = COMPETITION) {
  const replacement = entryPage().elements;
  replacement.entryForm.dataset.competitionId = competitionId;
  for (const id of ["entryForm", "submitEntry", "errorMessage", "successMessage"]) {
    elements[id] = replacement[id];
  }
}

test("payment completion updates the form reopened while its invoice was pending", async () => {
  const { sandbox, elements, modal, pay, tickets, entries, announce } = payingPlayer();
  const { done } = await pay();
  closeDialog(modal);
  replaceEntryForm(elements);
  await sandbox.submitEntry();
  assert.ok(elements.submitEntry.classList.contains("is-loading"));
  announce("fw:ticket-paid");
  await done;
  assert.equal(tickets().length, 1);
  assert.equal(entries().length, 1);
  assert.equal(elements.submitEntry.textContent, "Entered");
  assert.equal(elements.submitEntry.disabled, true);
  assert.ok(!elements.submitEntry.classList.contains("is-loading"));
  assert.ok(!elements.successMessage.classList.contains("hidden"));
});

test("payment failure releases the reopened form for retry", async () => {
  const { sandbox, elements, modal, pay, announce } = payingPlayer();
  const { done } = await pay();
  closeDialog(modal);
  replaceEntryForm(elements);
  await sandbox.submitEntry();
  announce("fw:ticket-failed", { message: "Ticket payment expired." });
  await done;
  assert.equal(elements.submitEntry.disabled, false);
  assert.ok(!elements.submitEntry.classList.contains("is-loading"));
  assert.equal(elements.errorMessage.textContent, "Ticket payment expired.");
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
});

test("payment completion leaves another competition's form unchanged", async () => {
  for (const outcome of ["fw:ticket-paid", "fw:ticket-failed"]) {
    const { elements, modal, pay, announce } = payingPlayer();
    const { done } = await pay();
    closeDialog(modal);
    replaceEntryForm(elements, "another-competition");
    announce(outcome, { message: "Ticket payment expired." });
    await done;
    assert.equal(elements.submitEntry.disabled, false);
    assert.equal(elements.submitEntry.textContent, "");
    assert.ok(elements.successMessage.classList.contains("hidden"));
    assert.ok(elements.errorMessage.classList.contains("hidden"));
  }
});

test("reopening enters the picks as they are when Pay is clicked again", async () => {
  const { sandbox, elements, modal, pay, settle, entries, announce } = payingPlayer();
  const { done: first } = await pay();
  closeDialog(modal);
  const picks = [{ name: "KPWM_temp_low", value: "under" }];
  elements.entryForm.querySelectorAll = () => picks;
  await sandbox.submitEntry();
  announce("fw:ticket-paid");
  await first;
  assert.deepEqual(JSON.parse(JSON.stringify(entries()[0].body.expected_observations)),
    [{ stations: "KPWM", temp_low: "Under" }]);
});

test("a ticket that expires unpaid gives Pay back for a new one", async () => {
  const { sandbox, elements, modal, pay, settle, tickets, announce } = payingPlayer();
  const { done: first } = await pay();
  closeDialog(modal);
  announce("fw:ticket-failed", { message: "Ticket payment expired. Please request a new ticket." });
  await first;
  assert.match(elements.errorMessage.textContent, /expired/);
  assert.equal(elements.submitEntry.disabled, false);
  assert.ok(!elements.submitEntry.classList.contains("is-loading"));
  await pay();
  assert.equal(tickets().length, 2, "a new ticket once the old one failed");
});

test("a paid ticket whose entry failed is entered on the next Pay, without paying again", async () => {
  let refuse = true;
  const { sandbox, elements, pay, settle, tickets, entries, announce } = payingPlayer({
    entry: () => (refuse
      ? { ok: false, status: 503, json: async () => ({ error: "database write was not accepted" }) }
      : { ok: true, json: async () => ({ id: "entry" }) }),
  });
  const { done: first } = await pay();
  announce("fw:ticket-paid");
  await first;
  assert.equal(elements.errorMessage.textContent, "Database write was not accepted; try again in a moment");
  assert.equal(elements.submitEntry.disabled, false);
  refuse = false;
  await sandbox.submitEntry();
  assert.equal(entries().length, 2);
  assert.equal(tickets().length, 1, "never a second ticket");
  assert.equal(elements.submitEntry.textContent, "Entered");
});

// Seen during a restart: the request never answered, then a 500, then a 503. Each says what
// to do, and none leaves the spinner on.
test("failed requests say to try again and give Pay back", async () => {
  for (const [label, ticket, message] of [
    ["unreachable", () => { throw new TypeError("Failed to fetch"); }, "Couldn't reach the server, try again"],
    ["a proxy with nothing behind it", () => ({ ok: false, status: 502, json: async () => { throw new SyntaxError("html"); } }),
      "Couldn't reach the server, try again"],
    ["a server error", () => ({ ok: false, status: 500, json: async () => ({ error: "internal server error" }) }),
      "Something went wrong on the server; try again in a moment"],
    ["the oracle unavailable", () => ({ ok: false, status: 503,
      json: async () => ({ error: "the oracle is unavailable right now; try again in a moment" }) }),
      "The oracle is unavailable right now; try again in a moment"],
    ["busy, with no reason", () => ({ ok: false, status: 503, json: async () => ({}) }),
      "The server is busy; try again in a moment"],
    ["an auth error's object", () => ({ ok: false, status: 503,
      json: async () => ({ error: { type: "replay_guard_full", detail: "full" } }) }),
      "The server is busy; try again in a moment"],
  ]) {
    const { elements, modal, pay, tickets } = payingPlayer({ ticket });
    await (await pay()).done;
    assert.equal(elements.errorMessage.textContent, message, label);
    assert.ok(!elements.errorMessage.classList.contains("hidden"), label);
    assert.equal(elements.submitEntry.disabled, false, label);
    assert.ok(!elements.submitEntry.classList.contains("is-loading"), label);
    assert.ok(!modal.classList.contains("is-active"), label);
    assert.equal(tickets().length, 1, label);
  }
});

test("terms that can't be fetched say to try again and give Pay back", async () => {
  const { elements, document } = entryPage();
  const sandbox = load(loggedIn({ dlcWallet: {} }), document, async () => { throw new TypeError("Failed to fetch"); });
  await sandbox.submitEntry();
  assert.equal(elements.errorMessage.textContent, "Couldn't reach the server, try again");
  assert.equal(elements.submitEntry.disabled, false);
  assert.ok(!elements.submitEntry.classList.contains("is-loading"));
});

// The coordinator's rules for one player's ticket requests: a request for the entry the ticket
// is reserved for gets that ticket and invoice back; one for another entry is refused with a
// 409 and releases the reservation. `lose` answers requests whose answer never arrives.
function reservingCoordinator() {
  const server = { reserved: null, issued: 0, lose: 0 };
  server.ticket = (body) => {
    const entryId = body.payout.entry_id;
    if (server.reserved && server.reserved !== entryId) {
      server.reserved = null;
      return { ok: false, status: 409, json: async () => ({ error: "your ticket was already requested with a different entry key or payout choice; that request has been cancelled, so request the ticket again" }) };
    }
    if (!server.reserved) {
      server.reserved = entryId;
      server.issued += 1;
    }
    const id = `ticket-${server.issued}`;
    if (server.lose > 0) {
      server.lose -= 1;
      throw new TypeError("Failed to fetch");
    }
    return { ok: true, json: async () => ({ ticket_id: id, payment_request: `lnbc53000n1${id}`, ...price(),
      keymeld_session_id: "session", keymeld_registration: { user_id: id, session_id: "session", payout_policy: "policy" } }) };
  };
  // The ticket is entered, or its reservation lapsed unpaid.
  server.entry = () => {
    server.reserved = null;
    return { ok: true, json: async () => ({ id: "entry" }) };
  };
  server.expire = () => { server.reserved = null; };
  return server;
}

function tabStorage() {
  const items = new Map();
  return {
    getItem: (key) => items.get(key) ?? null,
    setItem: (key, value) => items.set(key, String(value)),
    removeItem: (key) => items.delete(key),
  };
}

const entryIds = (tickets) => tickets().map(({ body }) => body.payout.entry_id);

test("a retry after an answer that never came sends the same entry and gets the same ticket", async () => {
  const server = reservingCoordinator();
  const sessionStorage = tabStorage();
  const { elements, modal, pay, tickets } = payingPlayer({ ticket: server.ticket, sessionStorage });
  server.lose = 1;
  await (await pay()).done;
  assert.equal(elements.errorMessage.textContent, "Couldn't reach the server, try again");
  assert.equal(elements.submitEntry.disabled, false);

  await pay();
  assert.ok(modal.classList.contains("is-active"), "the invoice is shown");
  assert.ok(elements.errorMessage.classList.contains("hidden"), "no 409");
  assert.equal(elements.walletLinkLightning.href, "lightning:lnbc53000n1ticket-1");
  assert.equal(server.issued, 1);
  const [first, retry] = entryIds(tickets);
  assert.equal(retry, first, "the same entry id");
  assert.deepEqual(tickets()[1].body, tickets()[0].body, "the same entry key and payout choice");

  // A reload within the reservation asks for the same ticket too.
  const reloaded = payingPlayer({ ticket: server.ticket, sessionStorage });
  await reloaded.pay();
  assert.ok(reloaded.modal.classList.contains("is-active"));
  assert.deepEqual(entryIds(reloaded.tickets), [first]);
  assert.equal(reloaded.elements.walletLinkLightning.href, "lightning:lnbc53000n1ticket-1");
});

test("after an entry is made, Pay starts a new entry", async () => {
  const server = reservingCoordinator();
  const sessionStorage = tabStorage();
  const { elements, pay, tickets, entries, announce } = payingPlayer({ ticket: server.ticket, entry: server.entry, sessionStorage });
  const { done } = await pay();
  // The reserving stub names its tickets ticket-1, ticket-2, … and the flow only listens for its own.
  announce("fw:ticket-paid", { ticket_id: "ticket-1" });
  await done;
  assert.equal(entries().length, 1);
  assert.equal(elements.submitEntry.textContent, "Entered");

  await pay();
  const [first, second] = entryIds(tickets);
  assert.notEqual(second, first, "a new entry id");
  assert.notEqual(tickets()[1].body.payout.payout_hash, undefined);
  assert.equal(server.issued, 2);
  assert.ok(elements.errorMessage.classList.contains("hidden"));
  // A reload after entering starts a new entry as well.
  const reloaded = payingPlayer({ ticket: server.ticket, entry: server.entry, sessionStorage });
  await reloaded.pay();
  assert.equal(entryIds(reloaded.tickets)[0], second, "the second entry, not the one entered");
});

test("an expired ticket renews the entry, and the next request gets a ticket", async () => {
  const server = reservingCoordinator();
  const { elements, modal, pay, tickets, announce } = payingPlayer({ ticket: server.ticket, sessionStorage: tabStorage() });
  const { done } = await pay();
  closeDialog(modal);
  server.expire();
  announce("fw:ticket-failed", { ticket_id: "ticket-1", message: "Ticket payment expired. Please request a new ticket." });
  await done;
  assert.match(elements.errorMessage.textContent, /expired/);

  await pay();
  const [first, second] = entryIds(tickets);
  assert.notEqual(second, first, "a new entry for the new ticket");
  assert.ok(modal.classList.contains("is-active"));
  assert.ok(elements.errorMessage.classList.contains("hidden"), "no 409");
  assert.equal(elements.walletLinkLightning.href, "lightning:lnbc53000n1ticket-2");
});


test("missing required picks are refused before login or payment", async () => {
  const { elements, document } = entryPage();
  elements.entryForm.dataset.maxValues = "3";
  const sandbox = load({ isLoggedIn: () => assert.fail("no login before complete picks") },
    document, async () => assert.fail("no request before complete picks"));
  await sandbox.submitEntry();
  assert.match(elements.errorMessage.textContent, /Make exactly 3 picks; you made 1/);
  assert.ok(!elements.errorMessage.classList.contains("hidden"));
});

// A competition may take fewer picks than it has rows; the counter above Pay says how many are
// left, and the first line matches what the server renders before any pick.
test("the counter says how many picks are left", () => {
  const { document } = entryPage();
  const { picksLeft } = load({}, document, termsFetch());
  assert.equal(picksLeft(0, 3, 12), "3 picks to make: any 3 of the 12 rows.");
  assert.equal(picksLeft(0, 12, 12), "12 picks to make: one in every row.");
  assert.equal(picksLeft(0, 1, 3), "1 pick to make: any 1 of the 3 rows.");
  assert.equal(picksLeft(1, 3, 12), "2 more to pick: 1 of 3 made.");
  assert.equal(picksLeft(3, 3, 12), "All 3 picks made.");
  assert.equal(picksLeft(1, 1, 3), "All 1 pick made.");
  assert.equal(picksLeft(5, 3, 12), "2 picks too many: take 2 back to make exactly 3.");
});

test("with fewer picks than rows, paying with none says how many of them", async () => {
  const { elements, document } = entryPage([]);
  elements.entryForm.dataset.maxValues = "3";
  elements.entryForm.dataset.pickRows = "12";
  elements.entryForm.querySelector = () => null;
  const sandbox = load({}, document, async () => assert.fail("nothing fetched"));
  await sandbox.submitEntry();
  assert.equal(elements.errorMessage.textContent, "Make exactly 3 picks, any 3 of the 12 rows, before paying.");
});

// A queued ticket's id is its entry's, so paying an unpaid ticket means asking for that entry's
// ticket again: the coordinator answers with the same ticket and invoice.
const UNPAID = "0190b6a0-0000-7000-8000-0000000000aa";

test("an unpaid ticket the form shows is paid with the picks on the form, under its own entry id", async () => {
  const server = reservingCoordinator();
  const { elements, modal, pay, tickets, entries, announce } = payingPlayer({ ticket: server.ticket, entry: server.entry, unpaid: UNPAID });
  const { done } = await pay();
  assert.ok(modal.classList.contains("is-active"), "its invoice is shown, not a refusal");
  assert.deepEqual(entryIds(tickets), [UNPAID]);
  assert.equal(elements.entryUnpaid.dataset.ticketId, undefined, "taken once");
  announce("fw:ticket-paid", { ticket_id: "ticket-1" });
  await done;
  assert.equal(entries().length, 1);
  assert.equal(entries()[0].body.id, UNPAID);
  assert.deepEqual(JSON.parse(JSON.stringify(entries()[0].body.expected_observations)), [{ stations: "KPWM", temp_high: "Over" }]);
  assert.equal(elements.submitEntry.textContent, "Entered");
});

test("a refusal for too many unpaid tickets pays the oldest of them instead", async () => {
  const server = reservingCoordinator();
  let refused = 0;
  const ticket = (body) => {
    if (body.payout.entry_id !== UNPAID) {
      refused++;
      return { ok: false, status: 400, json: async () => ({ error: "You have unpaid entries waiting in this competition; open its entry form and press Pay to pay one, or wait for its invoice to expire" }) };
    }
    return server.ticket(body);
  };
  const { elements, modal, pay, tickets } = payingPlayer({ ticket, listed: [
    { ticket_id: UNPAID, competition_id: COMPETITION, invoice_expires_at: "2026-10-06T12:00:00Z" },
  ] });
  await pay();
  assert.equal(refused, 1);
  assert.ok(modal.classList.contains("is-active"), "the oldest unpaid ticket's invoice is shown");
  assert.equal(entryIds(tickets).at(-1), UNPAID);
  assert.ok(elements.errorMessage.classList.contains("hidden"), "no refusal is shown");
});

test("with no unpaid ticket listed, the refusal is shown as it came", async () => {
  const ticket = () => ({ ok: false, status: 400, json: async () => ({ error: "You have unpaid entries waiting in this competition; open its entry form and press Pay to pay one, or wait for its invoice to expire" }) });
  const { elements, pay } = payingPlayer({ ticket });
  await (await pay()).done;
  assert.match(elements.errorMessage.textContent, /unpaid entries waiting/);
  assert.equal(elements.submitEntry.disabled, false);
});

test("the payment dialog counts down to the invoice's expiry", async () => {
  let ticks = null;
  let stopped = false;
  const timers = { setInterval: (tick) => { ticks = tick; return 1; }, clearInterval: () => { stopped = true; } };
  const expires = new Date(Date.now() + (41 * 60 + 30) * 1000).toISOString();
  const ticket = () => ({ ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "lnbc53000n1ticket",
    invoice_expires_at: expires, ...price(), keymeld_session_id: "session",
    keymeld_registration: { user_id: "ticket", session_id: "session", payout_policy: "policy" } }) });
  const { elements, pay, announce } = payingPlayer({ ticket, timers });
  const { done } = await pay();
  assert.match(elements.ticketPaymentExpiry.textContent, /^Invoice expires in 41:(29|30)$/);
  assert.equal(typeof ticks, "function");
  announce("fw:ticket-paid");
  await done;
  assert.ok(stopped, "the countdown stops with the dialog");
});

// A ticket paid for before the page reloaded, whose entry never went in. Its entry id is the one
// the wallet derived the entry key from; the payingPlayer wallet derives "pubkey" for any id.
const PAID = { ticketId: "paid-ticket", entryId: "0190b6a0-0000-7000-8000-0000000000bb", entryKey: "pubkey" };

test("a paid ticket the form shows is entered with the picks, without paying or registering again", async () => {
  const { sandbox, elements, tickets, entries, requests } = payingPlayer({ paid: PAID });
  await sandbox.submitEntry();
  assert.equal(tickets().length, 0, "nothing more to pay");
  assert.equal(requests.filter(({ url }) => url.endsWith("/registration")).length, 0);
  assert.equal(entries().length, 1);
  const body = entries()[0].body;
  assert.equal(body.id, PAID.entryId);
  assert.equal(body.ticket_id, PAID.ticketId);
  assert.equal(body.ephemeral_pubkey, "pubkey");
  assert.equal(body.payout_hash, "hash");
  assert.deepEqual(JSON.parse(JSON.stringify(body.expected_observations)), [{ stations: "KPWM", temp_high: "Over" }]);
  // The coordinator uses the registration sent before paying.
  assert.equal(body.encrypted_keymeld_private_key, null);
  assert.equal(body.keymeld_registration_context, null);
  assert.equal(elements.submitEntry.textContent, "Entered");
  assert.equal(elements.entryPaid.dataset.ticketId, undefined, "entered now");
});

test("a paid ticket whose entry is refused is entered on the next Pay, still without paying", async () => {
  let refuse = true;
  const { sandbox, elements, tickets, entries } = payingPlayer({
    paid: PAID,
    entry: () => (refuse
      ? { ok: false, status: 400, json: async () => ({ error: "Make exactly 1 pick" }) }
      : { ok: true, json: async () => ({ id: "entry" }) }),
  });
  await sandbox.submitEntry();
  assert.equal(elements.errorMessage.textContent, "Make exactly 1 pick");
  assert.equal(elements.submitEntry.disabled, false);
  refuse = false;
  await sandbox.submitEntry();
  assert.equal(entries().length, 2);
  assert.ok(entries().every(({ body }) => body.id === PAID.entryId && body.ticket_id === PAID.ticketId));
  assert.equal(tickets().length, 0);
  assert.equal(elements.submitEntry.textContent, "Entered");
});

test("a paid ticket under another account's key is not entered from this one", async () => {
  const { sandbox, elements, tickets, entries } = payingPlayer({ paid: { ...PAID, entryKey: "another" } });
  await sandbox.submitEntry();
  assert.equal(entries().length, 0);
  assert.equal(tickets().length, 0);
  assert.match(elements.errorMessage.textContent, /another account/);
});

test("Pay reads Enter while the form shows a paid ticket, and its price otherwise", () => {
  const { elements, document } = entryPage();
  const label = "Pay 5,300 sats and enter";
  Object.assign(elements.submitEntry, { textContent: label, dataset: { payLabel: label } });
  const sandbox = load({}, document, termsFetch());
  elements.entryPaid = element({ dataset: { ...PAID } });
  sandbox.labelPay();
  assert.equal(elements.submitEntry.textContent, "Enter");
  // Logged out: the notice is empty again.
  elements.entryPaid = element();
  sandbox.labelPay();
  assert.equal(elements.submitEntry.textContent, label);
  // A busy or finished button keeps its own.
  elements.entryPaid = element({ dataset: { ...PAID } });
  elements.submitEntry.textContent = "Entered";
  sandbox.labelPay();
  assert.equal(elements.submitEntry.textContent, "Entered");
});
