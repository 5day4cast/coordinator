// Uses a real, locally supplied WASM package (WASM_PACKAGE_DIR) and a
// generated throwaway wallet, in a phone-sized browser under the site's CSP.
// No account is created remotely; only the username-login HTTP response is
// stubbed. The page loads every script of the public bundle the way build.rs
// joins them (one private scope, base.js left out so no page setup runs), and
// the test drives them from inside that scope.
const assert = require('node:assert/strict');
const { readFileSync, readdirSync, statSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');

const templates = path.join(__dirname, '../../crates/coordinator/src/templates');
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'; trusted-types htmx";

// A signet invoice for exactly 21 sats, made 2026-09-24 and valid for 20
// years, signed by a throwaway key (0x42 repeated). Only its checks matter.
const VALID_INVOICE =
  'lntbs210n1p4ttx6jpp5kae7me7s8gzsfcuyuh0lv7vq6jcwam5yqqjun8g282kep9dxcqrssp5qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qursdp5vdhk7unyd9hxzar0wgsxyun0waek2u3qw3jhxapqve5hsar4wfjsxqxjespsqcqzys9qyysgq5recrzfn0kuqm2jt8lgn04tdfd6g06mc6c6p2npfz8wzwgrvhyap367x62vju7gwaul6n72kux9634kntrslpzdzza5x02ja503hk6qqfa8xw9';

// The public bundle's scripts in build.rs's order, without base.js.
function publicScripts() {
  const files = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const file = path.join(dir, name);
      if (statSync(file).isDirectory()) walk(file);
      else if (file.endsWith('.js') && name !== 'base.js') files.push(file);
    }
  };
  for (const dir of ['shared', 'components', 'fragments', 'pages', 'layouts']) walk(path.join(templates, dir));
  return files.map((file) => readFileSync(file, 'utf8')).join('\n;\n');
}

const walletScript = `(async () => {
${publicScripts()}
;
  const VALID = ${JSON.stringify(VALID_INVOICE)};
  const fail = (message) => { throw new Error(message); };
  try {
    await initWasm();
    await session.nostrClient.initialize(session.wasm.SignerType.PrivateKey, null);
    const credentials = session.wasm.LoginCredentials.derive('offline-browser-fixture', 'BrowserFixture123!');
    const authKey = credentials.authKey;
    const encrypted_nsec = session.nostrClient.sealForLogin(credentials);
    const wallet = session.wasm.DlcWallet.create(session.nostrClient, 'signet');

    // Invoices: amounts JavaScript cannot state exactly are refused, a bad
    // invoice reads "Invalid invoice", and the one right invoice is accepted.
    for (const amount of [NaN, Infinity, -1, 0, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
      let rejected;
      try { wallet.validateInvoice(VALID, amount); } catch (error) { rejected = String(error); }
      if (!/^Invalid invoice/.test(rejected ?? '')) fail('Wallet accepted amount ' + amount + ': ' + rejected);
    }
    let invalid;
    try { wallet.validateInvoice('invalid-invoice', 21); } catch (error) { invalid = String(error); }
    if (!/^Invalid invoice/.test(invalid ?? '') || /Keymeld/.test(invalid)) fail('Bad invoice error: ' + invalid);
    let wrongAmount;
    try { wallet.validateInvoice(VALID, 22); } catch (error) { wrongAmount = String(error); }
    if (!wrongAmount) fail('Wallet accepted a 21 sat invoice for 22 sats');
    wallet.validateInvoice(VALID, 21);
    // The QR code is drawn from the checked invoice only.
    const qr = wallet.invoiceQr(VALID, 21);
    if (!qr.startsWith('data:image/svg+xml;base64,')) fail('QR is not an SVG data URL');
    let qrRefused = false;
    try { wallet.invoiceQr(VALID, 22); } catch (_) { qrRefused = true; }
    if (!qrRefused) fail('Wallet drew a QR code for the wrong amount');
    const image = new Image();
    image.src = qr;
    await image.decode();
    // Entry ids are UUIDv7.
    const id = session.wasm.DlcWallet.newEntryId();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id)) fail('Entry id ' + id);

    const backup = await wallet.encryptedBackup();
    wallet.free(); credentials.free(); session.nostrClient.free();
    session.nostrClient = new session.wasm.NostrClientWrapper();
    const realFetch = window.fetch;
    window.fetch = async (url, options) => {
      if (String(url).endsWith('/api/v1/users/username/login')) {
        const body = JSON.parse(options.body);
        return new Response(JSON.stringify({ encrypted_nsec, ...backup, network: 'signet' }),
          { status: body.auth_key === authKey ? 200 : 401, headers: { 'Content-Type': 'application/json' } });
      }
      return realFetch(url, options);
    };
    const manager = new AuthManager(location.origin, 'signet');
    manager.attachEventListeners();
    document.body.addEventListener('fw:login', () => { document.body.dataset.loggedIn = String(isLoggedIn()); });
    document.body.dataset.ready = 'true';
  } catch (error) {
    document.body.dataset.failure = String(error);
  }
})();`;

test('real mobile wallet: invoices, QR, entry ids, and a login that stays in memory and off window',
  { skip: !process.env.WASM_PACKAGE_DIR }, async () => {
  const server = createServer((req, res) => {
    res.setHeader('Content-Security-Policy', policy);
    if (req.url.startsWith('/ui/pkg/')) {
      const file = path.basename(req.url.split('?')[0]);
      if (!['coordinator_wasm.js', 'coordinator_wasm_bg.wasm'].includes(file)) { res.statusCode = 404; return res.end(); }
      res.setHeader('Content-Type', file.endsWith('.wasm') ? 'application/wasm' : 'text/javascript');
      return res.end(readFileSync(path.join(process.env.WASM_PACKAGE_DIR, file)));
    }
    if (req.url === '/wallet.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(walletScript);
    }
    res.setHeader('Content-Type', 'text/html');
    res.end('<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><script src="/wallet.js" defer></script><body data-network="signet"><input id="loginUsername"><input id="loginPassword" type="password"><button id="usernameLoginButton">Log in</button><p id="usernameLoginError"></p><div id="authButtons"></div><div id="logoutContainer"></div></body>');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
    const errors = [];
    page.on('pageerror', (error) => errors.push(String(error)));
    // What a blank page has on window, before any of the site's scripts.
    await page.addInitScript(() => { window.__baseline = Object.getOwnPropertyNames(window); });
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(() => document.body.dataset.ready || document.body.dataset.failure, { timeout: 30000 });
    assert.equal(await page.getAttribute('body', 'data-failure'), null);
    await page.fill('#loginUsername', 'offline-browser-fixture');
    await page.fill('#loginPassword', 'BrowserFixture123!');
    await page.click('#usernameLoginButton');
    await page.waitForFunction(() => document.body.dataset.loggedIn === 'true' || document.querySelector('#usernameLoginError').textContent);
    assert.equal(await page.textContent('#usernameLoginError'), '');
    assert.equal(await page.getAttribute('body', 'data-logged-in'), 'true');
    assert.deepEqual(await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } })), { local: {}, session: {} });

    // Logged in, nothing a script injected into the page could reach leads
    // to the signer: the bundle adds only initWasm (which loads the module
    // and hands out no key) to window.
    const added = await page.evaluate(() => Object.getOwnPropertyNames(window)
      .filter((name) => !window.__baseline.includes(name) && name !== '__baseline'));
    assert.deepEqual(added.sort(), ['initWasm']);
    assert.equal(await page.evaluate(async () => (await window.initWasm()) ?? null), null);
    assert.deepEqual(errors, []);

    await page.reload();
    await page.waitForFunction(() => document.body.dataset.ready || document.body.dataset.failure);
    assert.equal(await page.getAttribute('body', 'data-logged-in'), null);
    await page.close();
  } finally {
    await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
