// Real-browser coverage for the shipped htmx version and account auth hook.
// Run: node --test browser/htmx-security.test.cjs (with Playwright installed).
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');
const templates = path.join(__dirname, '../../crates/coordinator/src/templates');
const htmx = readFileSync(path.join(templates, 'static/htmx-1.9.10.min.js'));
const hook = readFileSync(path.join(templates, 'shared/htmx_auth.js'), 'utf8');
const config = readFileSync(path.join(templates, 'layouts/base/mod.rs'), 'utf8').match(/HTMX_CONFIG: &str = r#"(.*?)"#;/)[1];
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

test('real htmx signs final GETs, keeps polls public and leaves no account history', async () => {
  const requests = [];
  const server = createServer((req, res) => {
    requests.push({ url: req.url, authorization: req.headers.authorization });
    res.setHeader('Content-Security-Policy', policy);
    if (req.url === '/htmx.js' || req.url === '/hook.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(req.url === '/htmx.js' ? htmx : `(()=>{window.signed=[];const session={nostrClient:{getAuthHeader:async(url,method,payload)=>{window.signed.push({url,method,payload});await new Promise(r=>setTimeout(r,15));return 'Nostr signed';}}};function isLoggedIn(){return true;} ${hook}\nsetupHtmxAuth();})();`);
    }
    res.setHeader('Content-Type', 'text/html');
    if (req.url.startsWith('/entries')) {
      res.setHeader('Cache-Control', 'private, no-store');
      return res.end('<p id="private">Private account result</p><script>window.injected=true</script><img src="/missing" onerror="window.injected=true">');
    }
    if (req.url.startsWith('/competitions/c1/leaderboard/rows')) return res.end('<p>Public scores</p>');
    if (req.url === '/missing') { res.statusCode = 404; return res.end(); }
    res.end(`<!doctype html><html><head><meta name="htmx-config" content='${config}'><script src="/htmx.js" defer></script><script src="/hook.js" defer></script></head><body><input id="city" name="city" value="Portland ME"><button id="account" hx-get="/entries?sort=new#private" hx-include="#city" hx-vals='{"page":2}' hx-target="#content" hx-push-url="true">Account</button><button id="public" hx-get="/competitions/c1/leaderboard/rows" hx-target="#scores">Scores</button><main id="content" hx-history-elt></main><div id="scores"></div></body></html>`);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    for (const viewport of [{ width: 1280, height: 800 }, { width: 390, height: 844 }]) {
      const page = await browser.newPage({ viewport });
      await page.addInitScript(() => localStorage.setItem('htmx-history-cache', '[{"content":"old private snapshot"}]'));
      await page.goto(origin);
      assert.equal(await page.evaluate(() => localStorage.getItem('htmx-history-cache')), null);
      await page.click('#account');
      await page.waitForSelector('#private');
      const signatures = await page.evaluate(() => window.signed);
      assert.equal(signatures.length, 1);
      assert.equal(signatures[0].url, `${origin}/entries?sort=new&city=Portland%20ME&page=2`);
      assert.equal(signatures[0].method, 'GET');
      assert.equal(requests.filter(r => r.url.startsWith('/entries')).at(-1).authorization, 'Nostr signed');
      await page.click('#public');
      await page.waitForFunction(() => document.querySelector('#scores').textContent.includes('Public scores'));
      assert.equal((await page.evaluate(() => window.signed)).length, 1);
      assert.equal(requests.filter(r => r.url.includes('/leaderboard/rows')).at(-1).authorization, undefined);
      const state = await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage }, injected: window.injected, config: htmx.config }));
      assert.deepEqual(state.local, {});
      assert.deepEqual(state.session, {});
      assert.equal(state.injected, undefined);
      assert.equal(state.config.historyCacheSize, 0);
      assert.equal(state.config.allowEval, false);
      assert.equal(state.config.allowScriptTags, false);
      assert.equal(state.config.selfRequestsOnly, true);
      await page.close();
    }
  } finally {
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
