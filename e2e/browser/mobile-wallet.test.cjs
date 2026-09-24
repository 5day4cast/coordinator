// Uses a real, locally supplied WASM package and a generated throwaway wallet.
// No account is created remotely; only the username-login HTTP response is stubbed.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');
const templates = path.join(__dirname, '../../crates/coordinator/src/templates');
const scripts = ['shared/wasm.js', 'shared/authorized_client.js', 'components/modals/modals.js'].map(file => readFileSync(path.join(templates, file), 'utf8')).join('\n');
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

test('real mobile wallet compiles and username login unlocks only in memory', { skip: !process.env.WASM_PACKAGE_DIR }, async () => {
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
      return res.end(`(async()=>{${scripts}
        try {
          await initWasm();
          await session.nostrClient.initialize(session.wasm.SignerType.PrivateKey, null);
          const credentials = session.wasm.LoginCredentials.derive('offline-browser-fixture','BrowserFixture123!');
          const authKey = credentials.authKey;
          const encrypted_nsec = session.nostrClient.sealForLogin(credentials);
          const wallet = session.wasm.DlcWallet.create(session.nostrClient, 'signet');
          const backup = await wallet.encryptedBackup();
          wallet.free(); credentials.free(); session.nostrClient.free();
          session.nostrClient = new session.wasm.NostrClientWrapper();
          const realFetch = window.fetch;
          window.fetch = async (url, options) => {
            if (String(url).endsWith('/api/v1/users/username/login')) {
              const body = JSON.parse(options.body);
              return new Response(JSON.stringify({encrypted_nsec, ...backup, network:'signet'}), {status:body.auth_key === authKey ? 200 : 401, headers:{'Content-Type':'application/json'}});
            }
            return realFetch(url, options);
          };
          const manager = new AuthManager(location.origin,'signet');
          manager.attachEventListeners();
          document.body.addEventListener('fw:login',()=>{document.body.dataset.loggedIn=String(isLoggedIn());});
          document.body.dataset.ready='true';
        } catch(error) { document.body.dataset.failure=String(error); }
      })();`);
    }
    res.setHeader('Content-Type', 'text/html');
    res.end('<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><script src="/wallet.js" defer></script><body data-network="signet"><input id="loginUsername"><input id="loginPassword" type="password"><button id="usernameLoginButton">Log in</button><p id="usernameLoginError"></p><div id="authButtons"></div><div id="logoutContainer"></div></body>');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const browser = await chromium.launch({ headless:true, ...(process.env.CHROMIUM_EXECUTABLE ? {executablePath:process.env.CHROMIUM_EXECUTABLE} : {}) });
  try {
    const page = await browser.newPage({viewport:{width:390,height:844}, isMobile:true, hasTouch:true});
    const errors = [];
    page.on('pageerror', error => errors.push(String(error)));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(() => document.body.dataset.ready || document.body.dataset.failure, {timeout:30000});
    assert.equal(await page.getAttribute('body','data-failure'), null);
    await page.fill('#loginUsername','offline-browser-fixture');
    await page.fill('#loginPassword','BrowserFixture123!');
    await page.click('#usernameLoginButton');
    await page.waitForFunction(() => document.body.dataset.loggedIn === 'true' || document.querySelector('#usernameLoginError').textContent);
    assert.equal(await page.textContent('#usernameLoginError'), '');
    assert.equal(await page.getAttribute('body','data-logged-in'), 'true');
    assert.deepEqual(await page.evaluate(() => ({local:{...localStorage},session:{...sessionStorage}, exposed:typeof window.nostrClient})), {local:{},session:{},exposed:'undefined'});
    assert.deepEqual(errors, []);
    await page.reload();
    await page.waitForFunction(() => document.body.dataset.ready || document.body.dataset.failure);
    assert.equal(await page.getAttribute('body','data-logged-in'), null);
    await page.close();
  } finally {
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
