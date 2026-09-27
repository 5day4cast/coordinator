const assert = require("node:assert/strict");
const test = require("node:test");
const { webcrypto } = require("node:crypto");
const { loadBundle } = require("./bundle.cjs");

const SCRIPTS = {
  entries: ["fragments/entry_form/entry_form.js", "Entry"],
  payouts: ["pages/payouts/payouts.js", "Payouts"],
};

// The bundle shares the wallet session and its other scripts' names between
// its scripts, inside one scope; a test hands them in with the page.
function load(name, page) {
  const session = { wasm: page, nostrClient: page.nostrClient ?? null, dlcWallet: page.dlcWallet ?? null };
  const [file, exported] = SCRIPTS[name];
  return loadBundle([file], { ...page, window: {}, session, crypto: webcrypto, TextEncoder, console }, [exported]);
}

for (const rejection of ["missing policy", "wrong ticket", "wrong session", "wallet rejects policy", null]) {
  test(`entry verifies payout consent before showing payment: ${rejection || "accepted"}`, async () => {
    let shown = 0;
    let ordinaryRegistrations = 0;
    let request;
    let registered = 0;
    const bundle = load("entries", {
      AuthorizedClient: class { async post(url, body) {
        if (url.endsWith("/registration")) {
          assert.equal(shown, 0, "the registration goes before the invoice is shown");
          registered++;
          return { ok: true, status: 204 };
        }
        request = body;
        return { ok: true, json: async () => ({ ticket_id: "ticket", payment_request: "ticket-invoice",
          keymeld_session_id: "session", keymeld_registration: {
            user_id: rejection === "wrong ticket" ? "another-ticket" : "ticket",
            session_id: rejection === "wrong session" ? "another-session" : "session",
            payout_policy: rejection === "missing policy" ? null : "policy" } }) };
      } },
      dlcWallet: {
        keymeldRegistration: () => { ordinaryRegistrations++; },
        keymeldPayoutRegistration: async (entry, assignment, serialized) => {
          const consent = JSON.parse(serialized);
          assert.equal(consent.lightning_address, "alice+prize@wallet.com");
          assert.equal(consent.ticket_invoice, "ticket-invoice");
          assert.equal(consent.expected_funding_sats, 1000);
          assert.equal(consent.max_fee_rate_sat_vb, 5);
          if (rejection) throw new Error("wallet refused substituted policy");
          return { encrypted_private_key: "ciphertext", auth_pubkey: "auth", context: {} };
        },
      },
    });
    const entry = new bundle.Entry("https://coordinator", "https://oracle", { id: "competition" });
    entry.entry = { id: "entry" };
    entry.payoutChoice = { entry_id: "entry", payout_hash: "own hash", lightning_address: "alice+prize@wallet.com",
      allow_invoice_fallback: true, release_entry_key_after_payment: true };
    entry.ticketAmountSats = 21;
    entry.payoutTerms = { competition: { event_submission: { total_competition_pool: 1000, total_allowed_entries: 2, number_of_places_win: 1 } },
      quote: { enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5 },
      oracle: { event_announcement: {} } };
    entry.showPaymentModal = async () => { shown++; };
    if (rejection) await assert.rejects(entry.handleTicketPayment("pubkey"));
    else await entry.handleTicketPayment("pubkey");
    assert.equal(shown, rejection ? 0 : 1);
    assert.equal(registered, rejection ? 0 : 1);
    assert.equal(ordinaryRegistrations, 0, "escrow consent must never downgrade to ordinary enrollment");
    assert.equal(request.payout.payout_hash, "own hash");
  });
}

function payoutFixture(status = 200, getBarrier = Promise.resolve()) {
  let releases = 0;
  let authorization;
  let posted;
  const bundle = load("payouts", {
    AuthorizedClient: class {
      async get() {
        await getBarrier;
        if (status !== 200) throw Object.assign(new Error("request failed"), { response: { status } });
        return { json: async () => ({ keygen_session_id: "session", user_id: "ticket", competition_id: "competition", entry_id: "entry",
          contract_digest: "contract digest", amount_msat: 42000, allow_invoice_fallback: true }) };
      }
      async post(url, body) { posted = { url, body }; return { ok: true }; }
    },
    dlcWallet: {
      // The wallet decodes invoices; this fixture's invoices are always valid.
      validateInvoice: (invoice, amount) => {
        assert.equal(amount, 42);
        if (!invoice) throw "Invalid invoice";
      },
      payoutRelease: () => { releases++; return { ephemeral_private_key: "legacy key", payout_preimage: "legacy preimage" }; },
      authorizePayoutInvoice: (serialized) => { authorization = JSON.parse(serialized); return { context: authorization.context, signature: [1, 2] }; },
    },
  });
  const payouts = new bundle.Payouts("https://coordinator", "https://oracle");
  const entry = { id: "entry", ephemeral_pubkey: "entry pubkey", ticket_id: "ticket" };
  const competition = { contract_parameters: {}, funding_outpoint: {}, signed_contract: { signatures: {} }, attestation: "attestation" };
  return { payouts, entry, competition, state: () => ({ releases, authorization, posted }) };
}

test("signed invoice fallback posts exact invoice authorization without entry secrets", async () => {
  const f = payoutFixture();
  await f.payouts.submitPayout("competition", f.entry, "ordinary-invoice", 42, f.competition);
  const { releases, authorization, posted } = f.state();
  assert.equal(releases, 0);
  assert.equal(authorization.invoice, "ordinary-invoice");
  assert.equal(authorization.context.invoice_digest.length, 64);
  assert.equal(posted.body.invoice, "ordinary-invoice");
  assert.deepEqual(Object.keys(posted.body).sort(), ["authorization", "invoice"]);
  assert.ok(posted.url.endsWith("/payout-authorization"));
});

for (const status of [404, 403, 500]) {
  test(`signed payout never releases secrets after authorization HTTP ${status}`, async () => {
    const f = payoutFixture(status);
    await assert.rejects(f.payouts.submitPayout("competition", f.entry, "ordinary-invoice", 42, f.competition));
    assert.equal(f.state().releases, 0);
    assert.equal(f.state().posted, undefined);
  });
}

for (const [consented, status, allowed] of [[false, 404, false], [true, 200, false], [true, 403, false], [true, 500, false], [true, 404, true]]) {
  test(`legacy recovery requires explicit consent and confirmed legacy status: ${consented}/${status}`, async () => {
    const f = payoutFixture(status);
    const action = f.payouts.submitLegacyPayout("competition", f.entry, "legacy-invoice", 42, consented);
    if (allowed) await action;
    else await assert.rejects(action);
    assert.equal(f.state().releases, allowed ? 1 : 0);
  });
}

test("invoice eligibility uses signed funding and whole sats for relative payout weights", async () => {
  const bundle = load("payouts", {});
  const payouts = new bundle.Payouts("https://coordinator", "https://oracle");
  payouts.getOracleEvent = async () => ({ attestation: "signed" });
  payouts.getCurrentOutcome = () => "att0";
  const competition = { id: "competition", attestation: "signed", event_submission: { total_competition_pool: 9999 },
    contract_parameters: { funding_value: 3000, players: [{ pubkey: "a" }, { pubkey: "b" }, { pubkey: "c" }],
      outcome_payouts: { att0: { 0: 1, 1: 1, 2: 1 } } } };
  for (const key of ["a", "b", "c"]) {
    const result = await payouts.checkEntryPayout({ event_id: "competition", ephemeral_pubkey: key }, [competition]);
    assert.equal(result.payout_amount, 1000, "equal stakes return equal contract shares");
  }
  competition.contract_parameters.funding_value = 3001;
  competition.contract_parameters.outcome_payouts.att0 = { 0: 70, 1: 30 };
  const result = await payouts.checkEntryPayout({ event_id: "competition", ephemeral_pubkey: "a" }, [competition]);
  assert.equal(result.payout_amount, 2100, "fractional sats use the server's integer division");
  competition.contract_parameters.outcome_payouts.att0 = { 0: 1, 7: 1 };
  assert.equal(await payouts.checkEntryPayout({ event_id: "competition", ephemeral_pubkey: "a" }, [competition]), null);
});

test("failed invoice attempts remain eligible for authorization; paid entries do not", async () => {
  const bundle = load("payouts", {});
  const payouts = new bundle.Payouts("https://coordinator", "https://oracle");
  payouts.getUserEntries = async () => [
    { id: "retry", payout_ln_invoice: "previous invoice", paid_out_at: null },
    { id: "paid", payout_ln_invoice: "paid invoice", paid_out_at: "yesterday" },
  ];
  payouts.getCompetitions = async () => [];
  payouts.checkEntryPayout = async entry => entry;
  assert.deepEqual(Array.from(await payouts.getPayableEntries(), entry => entry.id), ["retry"]);
});

function dialogFixture() {
  const classes = (...initial) => {
    const values = new Set(initial);
    return { contains: value => values.has(value), add: value => values.add(value), remove: value => values.delete(value),
      toggle: (value, force) => { if (force ?? !values.has(value)) values.add(value); else values.delete(value); } };
  };
  const elements = Object.fromEntries(["lightningInvoice", "payoutModalError", "legacyPayoutWarning", "legacyPayoutApproved", "payoutAmountSummary", "payoutModal", "submitPayoutInvoice"]
    .map(id => [id, { id, value: "", textContent: "", checked: false, disabled: false, classList: classes() }]));
  let resolveLookup;
  const submitted = [];
  let lookups = 0;
  const document = { getElementById: id => elements[id], querySelector: () => null };
  const bundle = loadBundle(["pages/payouts/payouts.js"], {
    document, console, window: { location: { reload() {} } },
    openModal: modal => modal.classList.add("is-active"),
    closeModal: modal => modal.classList.remove("is-active"),
  }, ["Payouts", "initPayouts", "openPayoutModal", "submitPayoutInvoice"]);
  bundle.Payouts.prototype.getPayableEntries = () => { lookups++; return new Promise(resolve => { resolveLookup = resolve; }); };
  bundle.Payouts.prototype.submitPayout = async (...args) => submitted.push(args);
  bundle.initPayouts("fixture", "fixture");
  const open = id => bundle.openPayoutModal({ dataset: { entryId: id, competitionId: "competition", payoutAmount: "1000" } });
  open("first");
  elements.lightningInvoice.value = "fixture invoice";
  return { ...bundle, open, elements, submitted, lookups: () => lookups,
    resolve: () => resolveLookup([{ entry: { id: "first" }, competition: { id: "competition" } }]) };
}

for (const change of ["closed", "different entry", "unchanged"]) {
  test(`pending payout lookup respects dialog state: ${change}`, async () => {
    const f = dialogFixture();
    const pending = f.submitPayoutInvoice();
    if (change === "closed") f.elements.payoutModal.classList.remove("is-active");
    if (change === "different entry") f.open("second");
    await f.submitPayoutInvoice();
    assert.equal(f.lookups(), 1, "repeated clicks cannot start duplicate submissions");
    f.resolve();
    await pending;
    assert.equal(f.submitted.length, change === "unchanged" ? 1 : 0);
    assert.equal(f.elements.submitPayoutInvoice.disabled, false);
  });
}

for (const legacy of [false, true]) {
  test(`closing during authorization lookup prevents ${legacy ? "legacy secret release" : "invoice signing"}`, async () => {
    let resume;
    const barrier = new Promise(resolve => { resume = resolve; });
    const f = payoutFixture(legacy ? 404 : 200, barrier);
    let active = true;
    const pending = legacy
      ? f.payouts.submitLegacyPayout("competition", f.entry, "invoice", 42, true, () => active)
      : f.payouts.submitPayout("competition", f.entry, "invoice", 42, f.competition, () => active);
    active = false;
    resume();
    await assert.rejects(pending, /cancelled/);
    assert.equal(f.state().releases, 0);
    assert.equal(f.state().authorization, undefined);
    assert.equal(f.state().posted, undefined);
  });
}

for (const legacy of [false, true]) {
  test(`closing during Nostr header signing prevents a delayed ${legacy ? "legacy" : "escrow"} POST`, async () => {
    let resume, notifySigning;
    const signing = new Promise(resolve => { notifySigning = resolve; });
    const barrier = new Promise(resolve => { resume = resolve; });
    const posted = [];
    const session = {
      nostrClient: { getAuthHeader: async (url, method) => {
        if (method === "POST") { notifySigning(); await barrier; }
        return "Nostr fixture";
      } },
      dlcWallet: { validateInvoice() {}, authorizePayoutInvoice: () => ({ signature: "fixture" }),
        payoutRelease: () => ({ payout_preimage: "fixture", ephemeral_private_key: "fixture" }) },
    };
    const { Payouts } = loadBundle(["shared/authorized_client.js", "pages/payouts/payouts.js"], {
      session, crypto: webcrypto, TextEncoder, console,
      fetch: async (url, options) => {
        if (options.method === "POST") { posted.push(options.body); return { ok: true }; }
        return { ok: !legacy, status: legacy ? 404 : 200, json: async () => ({
          entry_id: "entry", competition_id: "competition", user_id: "ticket",
          amount_msat: 42000, allow_invoice_fallback: true,
        }) };
      },
    }, ["Payouts"]);
    const payouts = new Payouts("https://coordinator", "https://oracle");
    const entry = { id: "entry", ticket_id: "ticket", ephemeral_pubkey: "entry key" };
    const competition = { contract_parameters: {}, funding_outpoint: {}, signed_contract: { signatures: {} }, attestation: "signed" };
    let active = true;
    const pending = legacy
      ? payouts.submitLegacyPayout("competition", entry, "invoice", 42, true, () => active)
      : payouts.submitPayout("competition", entry, "invoice", 42, competition, () => active);
    await signing;
    active = false;
    resume();
    await assert.rejects(pending, /cancelled/);
    assert.equal(posted.length, 0);
  });
}
