const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const ORIGIN = 'https://5day4cast.example';

function load({ loggedIn = true, now = () => 1_000_000, signer } = {}) {
  const listeners = {}, signed = [], opened = [], sent = [];
  const session = { nostrClient: { getAuthHeader: signer || (async (url, method, payload) => {
    signed.push({ url, method, payload });
    return `Nostr signature-${signed.length}`;
  }) } };
  const window = { location: { origin: ORIGIN }, openAuthModal: id => opened.push(id) };
  class FakeDate extends Date { static now() { return now(); } }
  vm.runInNewContext(readFileSync(path.join(__dirname, '../../crates/coordinator/src/templates/shared/htmx_auth.js'), 'utf8'), {
    window, session, isLoggedIn: () => loggedIn, URL, WeakMap, Date: FakeDate,
    console: { error() {} }, setTimeout: () => {},
    document: { body: { addEventListener: (name, fn) => (listeners[name] ??= []).push(fn), appendChild() {} },
      createElement: () => ({ append() {}, addEventListener() {} }) },
  });
  window.setupHtmxAuth();
  function fire(name, detail) {
    const event = { detail, target: detail.elt, prevented: false, preventDefault() { this.prevented = true; } };
    for (const fn of listeners[name] ?? []) fn(event);
    return event;
  }
  async function request(elt, verb, requestPath, parameters = {}, finalUrl) {
    let result;
    function issueRequest() {
      const headers = {};
      const config = fire('htmx:configRequest', { elt, verb, path: requestPath, headers, parameters });
      if (config.prevented) { result = { opened: false, headers }; return; }
      let url = requestPath;
      const pairs = Object.entries(parameters).flatMap(([k,v]) => (Array.isArray(v) ? v : [v]).map(v => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`));
      if (pairs.length) url += `${url.includes('?') ? '&' : '?'}${pairs.join('&')}`;
      const validate = fire('htmx:validateUrl', { elt, url: finalUrl || new URL(url, ORIGIN), headers });
      result = { opened: !validate.prevented, headers };
      if (result.opened) sent.push(url);
    }
    const confirm = fire('htmx:confirm', { elt, verb, path: requestPath, issueRequest });
    if (!confirm.prevented) issueRequest();
    await new Promise(resolve => setImmediate(resolve));
    return result || { opened: false, headers: {} };
  }
  return { fire, request, signed, opened, sent };
}

test('public fragments and polling do not ask the signer', async () => {
  const page = load();
  for (const url of ['/competitions/c1/leaderboard', '/competitions/c1/leaderboard/rows', '/entries/e1/detail']) {
    const request = await page.request({}, 'get', url);
    assert.equal(request.opened, true);
    assert.equal(request.headers.Authorization, undefined);
  }
  assert.equal(page.signed.length, 0);
});

test('signs htmx GET parameters exactly, including repeated values and existing query', async () => {
  const page = load();
  const request = await page.request({}, 'get', '/entries?sort=new', { city: ['Portland ME', 'Burlington'], page: 2 });
  assert.equal(request.opened, true);
  assert.equal(request.headers.Authorization, 'Nostr signature-1');
  assert.deepEqual(page.signed, [{ url: `${ORIGIN}/entries?sort=new&city=Portland%20ME&city=Burlington&page=2`, method: 'GET', payload: null }]);
});

test('a final URL changed after signing is never sent', async () => {
  const page = load();
  const result = await page.request({}, 'get', '/entries', {}, new URL('/entries?changed=yes', ORIGIN));
  assert.equal(result.opened, false);
});

test('mutations stay on the JSON client and cross-origin requests are not signed', async () => {
  const page = load();
  assert.equal((await page.request({}, 'post', '/payouts')).opened, false);
  assert.equal((await page.request({}, 'get', 'https://other.example/entries')).opened, false);
  assert.equal(page.signed.length, 0);
});

test('cancelled asynchronous signatures are dropped', async () => {
  let finish;
  const page = load({ signer: () => new Promise(resolve => { finish = resolve; }) });
  const elt = {};
  const result = await page.request(elt, 'get', '/entries');
  assert.equal(result.opened, false);
  page.fire('htmx:abort', { elt });
  finish('Nostr unused');
  await new Promise(resolve => setImmediate(resolve));
  const headers = {};
  page.fire('htmx:configRequest', { elt, verb: 'get', path: '/entries', headers, parameters: {} });
  assert.equal(headers.Authorization, undefined);
});

test('expired signatures do not resume requests', async () => {
  let finish, clock = 0;
  const page = load({ now: () => clock, signer: () => new Promise(resolve => { finish = resolve; }) });
  await page.request({}, 'get', '/entries');
  clock = 31_000;
  finish('Nostr stale');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(page.sent.length, 0);
});

test('logged-out account navigation prompts for login, public entry form still opens', async () => {
  const page = load({ loggedIn: false });
  assert.equal((await page.request({}, 'get', '/payouts')).opened, false);
  assert.deepEqual(page.opened, ['loginModal']);
  assert.equal((await page.request({}, 'get', '/competitions/c1/entry-form')).opened, true);
  assert.equal(page.signed.length, 0);
});
