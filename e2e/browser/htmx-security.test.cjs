// Real-browser coverage for the vendored htmx 4 with this site's two htmx
// extensions (shared/htmx_security.js and shared/htmx_auth.js), under the
// site's Content-Security-Policy with Trusted Types.
// Run: node --test browser/htmx-security.test.cjs (with Playwright installed).
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');

const root = path.join(__dirname, '../..');
const templates = path.join(root, 'crates/coordinator/src/templates');
const htmx = readFileSync(path.join(root, 'vendor/htmx/4.0.0/htmx.min.js'));
const config = readFileSync(path.join(templates, 'layouts/base/mod.rs'), 'utf8').match(/HTMX_CONFIG: &str = r#"(.*?)"#;/)[1];
// As public_headers.rs builds it for the public pages.
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'; trusted-types htmx login-worker";

// The two extensions as build.rs bundles them: inside one function, with a
// wallet session that records what it signs.
const extensions = `(() => {
  window.signed = [];
  const session = { nostrClient: { getAuthHeader: async (url, method, payload) => {
    window.signed.push({ url, method, payload });
    await new Promise((resolve) => setTimeout(resolve, 15));
    return 'Nostr signed-' + window.signed.length;
  } } };
  function isLoggedIn() { return true; }
  function openAuthModal() {}
  ${readFileSync(path.join(templates, 'shared/htmx_security.js'), 'utf8')}
  ${readFileSync(path.join(templates, 'shared/htmx_auth.js'), 'utf8')}
  setupHtmxAuth();
})();`;

// Everything a response could try to run once htmx swaps it in.
const injected = `<p id="payload">Payload swapped in</p>
<script>window.ran = (window.ran || []).concat('inline script')</script>
<img src="/missing" onerror="window.ran = (window.ran || []).concat('onerror')">
<button id="hxOn" hx-on:click="window.ran = (window.ran || []).concat('hx-on')">hx-on</button>
<button id="filter" hx-get="/competitions/c1/leaderboard/rows" hx-target="#scores" hx-trigger="click[window.ran = (window.ran || []).concat('trigger filter')]">filter</button>
<button id="jsVals" hx-get="/competitions/c1/leaderboard/rows" hx-target="#scores" hx-vals="js:{a: (window.ran = (window.ran || []).concat('js: value'))}">js vals</button>`;

function page(main = '') {
  return `<!doctype html><html><head><meta name="htmx-config" content='${config}'>
<script src="/htmx.js" defer></script><script src="/extensions.js" defer></script></head>
<body><input id="city" name="city" value="Portland ME">
<button id="account" hx-get="/entries?sort=new#private" hx-include="#city" hx-vals='{"page":2}' hx-target="#content" hx-push-url="true">Account</button>
<a id="leaderboard" href="/competitions/c1/leaderboard" hx-get="/competitions/c1/leaderboard" hx-target="#content" hx-push-url="true">Leaderboard</a>
<button id="public" hx-get="/competitions/c1/leaderboard/rows" hx-target="#scores">Scores</button>
<a id="payouts" href="/payouts" hx-get="/payouts" hx-target="#content" hx-push-url="true">Payouts</a>
<button id="watchTicket" hx-get="/ticket-poller" hx-target="#ticket">Watch ticket</button>
<button id="watchBoard" hx-get="/board-poller" hx-target="#live">Watch board</button>
<div id="ticket"></div><div id="live"></div>
<button id="inject" hx-get="/inject" hx-target="#injected">Inject</button>
<button id="injectSrc" hx-get="/inject-src" hx-target="#injected">Inject a script file</button>
<main id="content" hx-history-elt>${main}</main><div id="scores"></div><div id="injected"></div></body></html>`;
}

test('htmx 4 signs exact account GETs, never polls, stores nothing and runs no injected code', async () => {
  const requests = [];
  const server = createServer((req, res) => {
    requests.push({ url: req.url, authorization: req.headers.authorization, restore: req.headers['hx-history-restore-request'] });
    res.setHeader('Content-Security-Policy', policy);
    if (req.url === '/htmx.js' || req.url === '/extensions.js' || req.url === '/evil.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(req.url === '/htmx.js' ? htmx
        : req.url === '/evil.js' ? "window.ran = (window.ran || []).concat('script src')" : extensions);
    }
    if (req.url === '/missing') { res.statusCode = 404; return res.end(); }
    res.setHeader('Content-Type', 'text/html');
    const full = !req.headers['hx-request'] || req.headers['hx-history-restore-request'];
    if (req.url.startsWith('/entries')) {
      res.setHeader('Cache-Control', 'private, no-store');
      const content = '<p id="private">Private account result</p>';
      return res.end(full ? page(content) : content);
    }
    if (req.url === '/competitions/c1/leaderboard') {
      const content = '<p id="board">Public leaderboard</p>';
      return res.end(full ? page(content) : content);
    }
    // A ticket's status polls every second, signed, until it is paid; the
    // live board polls unsigned until the window closes.
    // As entry_form/mod.rs renders it: a tick while a request is out is
    // dropped, not queued behind it.
    const poller = (url, text) => `<div class="poller" hx-get="${url}" hx-trigger="every 1s" hx-sync="drop" hx-swap="outerHTML">${text}</div>`;
    if (req.url === '/ticket-poller') return res.end(poller('/competitions/c1/tickets/t1/status', 'Waiting'));
    if (req.url === '/competitions/c1/tickets/t1/status') {
      const polls = requests.filter((r) => r.url === req.url).length;
      if (polls < 3) return res.end(poller(req.url, 'Waiting'));
      res.setHeader('HX-Trigger', 'fw:ticket-paid');
      return res.end('<div id="paid">Payment received!</div>');
    }
    if (req.url === '/board-poller') return res.end(poller('/competitions/c1/leaderboard/rows?live=1', 'Scores so far'));
    if (req.url === '/competitions/c1/leaderboard/rows?live=1') {
      const polls = requests.filter((r) => r.url === req.url).length;
      return res.end(polls < 2 ? poller(req.url, 'Scores so far') : '<div id="closed">Window closed</div>');
    }
    if (req.url.startsWith('/competitions/c1/leaderboard/rows')) return res.end('<p>Public scores</p>');
    if (req.url.startsWith('/payouts')) {
      res.setHeader('Cache-Control', 'private, no-store');
      const content = '<p id="payoutsPage">Private payouts</p>';
      return res.end(full ? page(content) : content);
    }
    if (req.url === '/inject') return res.end(injected);
    if (req.url === '/inject-src') return res.end('<p id="scriptPayload">Script file</p><script src="/evil.js"></script>');
    res.end(page());
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    for (const viewport of [{ width: 1280, height: 800 }, { width: 390, height: 844 }]) {
      requests.length = 0;
      const tab = await browser.newPage({ viewport });
      const errors = [];
      tab.on('pageerror', (error) => errors.push(String(error)));
      // A page snapshot an htmx 1.x release could have left behind.
      await tab.addInitScript(() => localStorage.setItem('htmx-history-cache', '[{"content":"old private snapshot"}]'));
      await tab.goto(origin);
      await tab.waitForFunction(() => window.htmx?.version === '4.0.0' && Array.isArray(window.signed));
      assert.equal(await tab.evaluate(() => localStorage.getItem('htmx-history-cache')), null);

      // The account request is signed once, for exactly the URL the server got.
      await tab.click('#account');
      await tab.waitForSelector('#private');
      const account = requests.find((r) => r.url.startsWith('/entries'));
      assert.equal(account.url, '/entries?sort=new&city=Portland+ME&page=2');
      assert.equal(account.authorization, 'Nostr signed-1');
      assert.deepEqual(await tab.evaluate(() => window.signed), [{ url: `${origin}${account.url}`, method: 'GET', payload: null }]);

      // Public refreshes are never signed.
      await tab.click('#public');
      await tab.waitForFunction(() => document.querySelector('#scores').textContent.includes('Public scores'));
      assert.equal(requests.filter((r) => r.url.includes('/leaderboard/rows')).at(-1).authorization, undefined);

      // The payouts page is signed too.
      await tab.click('#payouts');
      await tab.waitForSelector('#payoutsPage');
      assert.equal(requests.filter((r) => r.url === '/payouts').at(-1).authorization, 'Nostr signed-2');
      await tab.goBack();
      await tab.waitForSelector('#private');
      assert.equal(requests.filter((r) => r.restore).at(-1).authorization, 'Nostr signed-3');

      // A ticket's status: each poll signed afresh; the paid answer stops the
      // polling and tells the page (HX-Trigger).
      await tab.evaluate(() => document.addEventListener('fw:ticket-paid', () => { window.ticketPaid = true; }));
      await tab.click('#watchTicket');
      await tab.waitForSelector('#paid', { timeout: 10000 });
      assert.equal(await tab.evaluate(() => window.ticketPaid), true);
      const ticketPolls = requests.filter((r) => r.url === '/competitions/c1/tickets/t1/status');
      assert.equal(ticketPolls.length, 3);
      assert.deepEqual(ticketPolls.map((r) => r.authorization), ['Nostr signed-4', 'Nostr signed-5', 'Nostr signed-6']);

      // A live board polls unsigned and stops once the window has closed.
      await tab.click('#watchBoard');
      await tab.waitForSelector('#closed', { timeout: 10000 });
      await tab.waitForTimeout(1500);
      const boardPolls = requests.filter((r) => r.url === '/competitions/c1/leaderboard/rows?live=1');
      assert.equal(boardPolls.length, 2);
      assert.ok(boardPolls.every((r) => r.authorization === undefined));
      assert.equal(requests.filter((r) => r.url === '/competitions/c1/tickets/t1/status').length, 3, 'the ticket stopped polling');

      // Back to the account page: htmx asks the server again, signed, and
      // keeps no copy of the page anywhere.
      await tab.click('#leaderboard');
      await tab.waitForSelector('#board');
      await tab.goBack();
      await tab.waitForSelector('#private');
      const restore = requests.filter((r) => r.restore).at(-1);
      assert.equal(restore.url, account.url);
      assert.equal(restore.authorization, 'Nostr signed-7');
      assert.deepEqual(await tab.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } })), { local: {}, session: {} });

      // Nothing in a swapped response runs: scripts, handlers, hx-on,
      // trigger filters or js: values.
      await tab.click('#inject');
      await tab.waitForSelector('#payload');
      await tab.click('#hxOn');
      await tab.click('#filter');
      await tab.click('#jsVals');
      await tab.waitForTimeout(300);
      assert.equal(await tab.evaluate(() => window.ran), undefined);
      // A response carrying a script file is refused whole: its src needs a
      // TrustedScriptURL, which no policy on the page makes.
      await tab.click('#injectSrc');
      await tab.waitForTimeout(300);
      assert.equal(await tab.$('#scriptPayload'), null);
      assert.ok(!requests.some((r) => r.url === '/evil.js'), 'a swapped <script src> is never fetched');
      assert.equal(await tab.evaluate(() => window.ran), undefined);

      // Trusted Types are enforced: nothing but htmx's policy turns a string
      // into HTML, and no second policy can be made.
      assert.deepEqual(await tab.evaluate(() => {
        const attempt = (fn) => { try { fn(); return 'allowed'; } catch (error) { return error.name; } };
        return {
          innerHTML: attempt(() => { document.querySelector('#scores').innerHTML = '<b>x</b>'; }),
          policy: attempt(() => trustedTypes.createPolicy('another', { createHTML: (s) => s })),
        };
      }), { innerHTML: 'TypeError', policy: 'TypeError' });

      assert.ok(errors.every((error) => /inline JavaScript is disabled|Trusted|TrustedScript/.test(error)), errors.join('\n'));
      await tab.close();
    }
  } finally {
    await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
