// Exercise the real auth and handoff click listeners together. Open Satchel
// shares the navbar container with Log out; only the latter must clear the signer.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');
const source = readFileSync(path.join(__dirname, '../../crates/coordinator/src/templates/shared/satchel.js'), 'utf8');
const authSource = readFileSync(path.join(__dirname, '../../crates/coordinator/src/templates/components/modals/modals.js'), 'utf8');
const listen = async (server) => { await new Promise(resolve => server.listen(0, '127.0.0.1', resolve)); return `http://127.0.0.1:${server.address().port}`; };

test('Open Satchel and Pay with Satchel navigate the tab after asynchronous signing', async () => {
  const posts = [];
  const wallet = createServer(async (req, res) => {
    if (req.method === 'POST') {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const form = new URLSearchParams(Buffer.concat(chunks).toString());
      posts.push({ path: req.url, event: JSON.parse(form.get('event')), next: form.get('next') });
      res.writeHead(303, { Location: form.get('next') }); return res.end();
    }
    res.setHeader('Content-Type', 'text/html'); res.end('<!doctype html><h1>Wallet destination</h1>');
  });
  const walletOrigin = await listen(wallet);
  const script = `(() => {
    const session = { nostrClient: {
      active: true,
      isSignerReady: () => true,
      signHandoff: async function () { if (!this.active) throw new Error("signer was freed by logout"); return JSON.stringify({kind: 27235}); }
    } };
    class AuthorizedClient { async post() { await new Promise(r => setTimeout(r, 25)); return {json: async () => ({username: 'alice'})}; } }
    ${authSource}
    ${source}
    const manager = new AuthManager('', 'signet');
    manager.handleLogout = async () => {
      // Real logout first waits for IndexedDB, then frees the signer.
      await new Promise(r => setTimeout(r, 5));
      session.nostrClient.active = false;
      document.body.dataset.loggedOut = 'true';
    };
    manager.attachEventListeners();
    setupSatchel();
  })();`;
  const site = createServer((req, res) => {
    res.setHeader('Content-Security-Policy', `default-src 'none'; script-src 'self'; form-action 'self' ${walletOrigin}; require-trusted-types-for 'script'; trusted-types htmx login-worker pow-worker`);
    if (req.url === '/handoff.js') { res.setHeader('Content-Type', 'text/javascript'); return res.end(script); }
    res.setHeader('Content-Type', 'text/html');
    res.end(`<!doctype html><body data-satchel-url="${walletOrigin}"><script src="/handoff.js" defer></script><div id="logoutContainer"><a id="open" href="${walletOrigin}/wallet" data-satchel-next="/wallet">Open Satchel</a><button id="logoutNavClick">Log out</button></div><a id="pay" href="${walletOrigin}/launch/lightning/lntbs1fixture" data-satchel-next="/launch/lightning/lntbs1fixture">Pay with Satchel</a></body>`);
  });
  const origin = await listen(site);
  const browser = await chromium.launch({headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? {executablePath: process.env.CHROMIUM_EXECUTABLE} : {})});
  try {
    const page = await browser.newPage();
    await page.goto(origin);
    const opened = page.waitForEvent('popup');
    await page.click('#open');
    const popup = await opened;
    await popup.waitForURL(walletOrigin + '/wallet', {timeout: 5000});
    assert.match(await popup.textContent('body'), /Wallet destination/);
    await page.click('#pay');
    await popup.waitForURL(walletOrigin + '/launch/lightning/lntbs1fixture', {timeout: 5000});
    assert.deepEqual(posts.map(p => p.next), ['/wallet', '/launch/lightning/lntbs1fixture']);
    assert.ok(posts.every(p => p.path === '/auth/nostr/handoff' && p.event.kind === 27235));
    assert.equal(await page.getAttribute('body', 'data-logged-out'), null, 'opening or paying with Satchel keeps the game login');
    await page.click('#logoutNavClick');
    await page.waitForFunction(() => document.body.dataset.loggedOut === 'true');
    assert.equal(posts.length, 2, 'Log out does not start a wallet handoff');
  } finally {
    await browser.close();
    await Promise.all([new Promise(resolve => site.close(resolve)), new Promise(resolve => wallet.close(resolve))]);
  }
});
