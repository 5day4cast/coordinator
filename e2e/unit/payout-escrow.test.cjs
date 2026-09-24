const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");

function load(name, window) {
  vm.runInNewContext(readFileSync(path.join(__dirname,
    `../../crates/coordinator/src/templates/pages/${name}/${name}.js`), "utf8"),
  { window, crypto: webcrypto, TextEncoder, console });
  return window;
}

for (const rejection of ["missing policy", "wrong ticket", "wrong session", "wallet rejects policy", null]) {
  test(`entry verifies payout consent before showing payment: ${rejection || "accepted"}`, async () => {
    let shown = 0;
    let ordinaryRegistrations = 0;
    let request;
    const window = load("entries", {
      AuthorizedClient: class { async post(url, body) {
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
    const entry = new window.Entry("https://coordinator", "https://oracle", { id: "competition" });
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
    assert.equal(ordinaryRegistrations, 0, "escrow consent must never downgrade to ordinary enrollment");
    assert.equal(request.payout.payout_hash, "own hash");
  });
}

function payoutFixture(status = 200) {
  let releases = 0;
  let authorization;
  let posted;
  const window = load("payouts", {
    AuthorizedClient: class {
      async get() {
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
  const payouts = new window.Payouts("https://coordinator", "https://oracle");
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
