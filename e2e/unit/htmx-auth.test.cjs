// The NIP-98 hook (shared/htmx_auth.js) as an htmx 4 extension, driven with
// the request context htmx hands it. e2e/browser/htmx-security.test.cjs runs
// it against the real htmx.
const assert = require('node:assert/strict');
const test = require('node:test');
const { loadBundle } = require('./bundle.cjs');
const ORIGIN = 'https://5day4cast.example';

function load({ loggedIn = true, signer } = {}) {
  const signed = [], opened = [], sent = [], errors = [];
  const state = { loggedIn };
  const session = { nostrClient: { getAuthHeader: signer || (async (url, method, payload) => {
    signed.push({ url, method, payload });
    return `Nostr signature-${signed.length}`;
  }) } };
  let extension;
  const window = {
    location: { origin: ORIGIN },
    htmx: { registerExtension: (name, hooks) => { assert.equal(name, 'fw-auth'); extension = hooks; } },
  };
  loadBundle(['shared/htmx_auth.js'], {
    window, session, isLoggedIn: () => state.loggedIn, URL,
    openAuthModal: (id) => opened.push(id),
    console: { error: (message) => errors.push(message) },
    setTimeout: () => 0, clearTimeout: () => {},
    document: { baseURI: `${ORIGIN}/competitions`, body: { addEventListener() {}, appendChild() {} },
      createElement: () => ({ append() {}, addEventListener() {} }) },
  }, []);
  assert.deepEqual(Object.keys(window), ['location', 'htmx'], 'nothing is put on window');

  // What htmx 4 does for one request: before:request, then ctx.fetch(action, request).
  async function request(action, { method = 'GET', elt = {}, headers = {}, sentAction = action } = {}) {
    const ctx = {
      request: { action, method, headers: { 'HX-Request': 'true', ...headers }, abort() {} },
      fetch: async (url, init) => { sent.push({ url, authorization: init.headers.Authorization }); return { ok: true }; },
    };
    if (extension.htmx_before_request(elt, { ctx }) === false) return { sent: false };
    try {
      await ctx.fetch(sentAction, { method, headers: ctx.request.headers });
      return { sent: true };
    } catch (error) {
      return { sent: false, error };
    }
  }
  return { request, extension, state, session, signed, opened, sent, errors };
}

test('public fragments and polling never ask the signer', async () => {
  const page = load();
  for (const action of ['/competitions', '/competitions/c1/leaderboard/rows', '/entries/e1/detail', '/competitions/c1/entry-forecasts']) {
    assert.deepEqual(await page.request(action), { sent: true });
  }
  assert.deepEqual(page.signed, []);
  assert.ok(page.sent.every((request) => request.authorization === undefined));
});

test('an account page is signed for exactly the URL and query htmx sends', async () => {
  const page = load();
  const action = '/entries?sort=new&city=Portland%20ME&city=Bangor&page=2';
  assert.deepEqual(await page.request(`${action}#private`), { sent: true });
  assert.deepEqual(page.signed, [{ url: `${ORIGIN}${action}`, method: 'GET', payload: null }]);
  assert.equal(page.sent[0].authorization, 'Nostr signature-1');
});

test('a ticket status poll is signed; so is the entry form when logged in', async () => {
  const page = load();
  await page.request('/competitions/c1/tickets/t1/status');
  await page.request('/competitions/c1/entry-form');
  assert.deepEqual(page.signed.map((s) => s.url), [
    `${ORIGIN}/competitions/c1/tickets/t1/status`,
    `${ORIGIN}/competitions/c1/entry-form`,
  ]);
});

test('a signature is never sent to a URL other than the one signed', async () => {
  const page = load();
  const result = await page.request('/payouts', { sentAction: '/payouts?stolen=1' });
  assert.equal(result.sent, false);
  assert.deepEqual(page.signed, []);
  assert.deepEqual(page.sent, []);
});

test('mutations stay on the JSON client and cross-origin requests are not signed', async () => {
  const page = load();
  assert.deepEqual(await page.request('/entries', { method: 'POST' }), { sent: false });
  assert.deepEqual(await page.request('https://evil.example/entries'), { sent: false });
  assert.deepEqual(page.signed, []);
  assert.equal(page.errors.length, 2);
});

test('logging out while the wallet signs sends nothing', async () => {
  let release;
  const page = load({ signer: () => new Promise((resolve) => { release = resolve; }) });
  const pending = page.request('/entries');
  await new Promise((resolve) => setImmediate(resolve));
  page.state.loggedIn = false;
  release('Nostr late');
  const result = await pending;
  assert.equal(result.sent, false);
  assert.match(result.error.message, /account changed/);
  assert.deepEqual(page.sent, []);
});

test('a refused signature is reported and nothing is sent', async () => {
  const page = load({ signer: async () => { throw new Error('user rejected'); } });
  const result = await page.request('/payouts');
  assert.equal(result.sent, false);
  assert.deepEqual(page.sent, []);
});

test('logged-out account navigation asks for a login; the public entry form still opens', async () => {
  const page = load({ loggedIn: false });
  assert.deepEqual(await page.request('/entries'), { sent: false });
  assert.deepEqual(page.opened, ['loginModal']);
  assert.deepEqual(await page.request('/competitions/c1/entry-form'), { sent: true });
  // Back to an account page while logged out: its log-in prompt, unsigned.
  assert.deepEqual(await page.request('/entries', { headers: { 'HX-History-Restore-Request': 'true' } }), { sent: true });
  assert.deepEqual(page.signed, []);
});

test('a 401 asks the player to log in again', () => {
  const page = load();
  page.extension.htmx_response_error({}, { ctx: { response: { status: 401 } } });
  page.extension.htmx_response_error({}, { ctx: { response: { status: 500 } } });
  assert.deepEqual(page.opened, ['loginModal']);
});
