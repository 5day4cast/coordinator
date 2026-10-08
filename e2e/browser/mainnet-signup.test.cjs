// Render fixtures with cargo run -p coordinator --example mainnet_signup_fixtures -- <dir>.
// Exercises the real templates, worker, htmx swaps, and shared modal behavior under CSP.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const { createHash, randomBytes } = require('node:crypto');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');
const fixtures = process.env.MAINNET_SIGNUP_FIXTURES;
const templates = path.join(__dirname, '../../crates/coordinator/src/templates');
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'; trusted-types htmx login-worker pow-worker";

test('mainnet signup works in the dialog, after a refused proof, and without JavaScript', { skip: !fixtures }, async () => {
  const scripts = ['shared/htmx_security.js', 'shared/modal_utils.js', 'components/modals/signup_pow.js', 'components/feedback/feedback.js', 'components/mainnet_signup/mainnet_signup.js']
    .map(file => readFileSync(path.join(templates, file), 'utf8')).join('\n');
  const script = `(() => { ${scripts}\n setupModalCloseHandlers(); setupFeedback(); setupMainnetSignup(); })();`;
  const markup = readFileSync(path.join(fixtures, 'index.html'), 'utf8')
    .replace(/<script src="[^"]*\/app\.[^"]*" defer><\/script>/, '<script src="/signup-test.js" defer></script>')
    .replace('fw-auth, fw-security', 'fw-security');
  assert.ok(markup.includes('/signup-test.js'));
  const submitted = [];
  let refuseNext = true;
  let challengeCount = 0;
  const server = createServer(async (req, res) => {
    res.setHeader('Content-Security-Policy', policy);
    if (req.url === '/signup-test.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(script);
    }
    if (req.url.startsWith('/assets/')) {
      res.setHeader('Content-Type', req.url.endsWith('.js') ? 'text/javascript' : 'text/css');
      return res.end(readFileSync(path.join(fixtures, req.url)));
    }
    if (req.url === '/api/v1/mainnet-signup/challenge') {
      challengeCount++;
      res.setHeader('Content-Type', 'application/json');
      return res.end(JSON.stringify({ challenge: randomBytes(32).toString('base64url'), difficulty: 8 }));
    }
    if (req.method === 'POST') {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      submitted.push({ url: req.url, form: new URLSearchParams(Buffer.concat(chunks).toString()) });
      res.setHeader('Content-Type', 'text/html');
      if (refuseNext) {
        refuseNext = false;
        return res.end(readFileSync(path.join(fixtures, 'retry.html')));
      }
      return res.end(readFileSync(path.join(fixtures, 'thanks.html')));
    }
    res.setHeader('Content-Type', 'text/html');
    res.end(markup);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    for (const { viewport, colorScheme } of [
      { viewport: { width: 1280, height: 800 }, colorScheme: "light" },
      { viewport: { width: 390, height: 844 }, colorScheme: "light" },
      { viewport: { width: 1280, height: 800 }, colorScheme: "dark" },
      { viewport: { width: 390, height: 844 }, colorScheme: "dark" },
    ]) {
      refuseNext = true;
      const page = await browser.newPage({ viewport, colorScheme });
      const errors = [];
      page.on('pageerror', error => errors.push(String(error)));
      await page.goto(origin);
      await page.locator('footer [data-mainnet-signup-open]').click();
      const modal = page.locator('#mainnetSignupModal');
      await page.waitForFunction(() => document.activeElement?.name === 'email');
      await modal.locator('[name=email]').fill('player@example.com');
      await modal.locator('button[type=submit]').click();
      await modal.getByRole('alert').waitFor();
      assert.equal(await modal.locator('[name=email]').inputValue(), 'player@example.com');
      await modal.locator('button[type=submit]').click();
      await modal.getByRole('status').waitFor();
      assert.match(await modal.innerText(), /We'll email you when 5day4cast goes live on mainnet/);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      await page.screenshot({ path: path.join(fixtures, `signup-${viewport.width}-${colorScheme}.png`) });
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('footer [data-mainnet-signup-open]').evaluate(el => el === document.activeElement), true);
      await page.locator('footer [data-mainnet-signup-open]').click();
      assert.equal(await modal.locator('[name=email]').inputValue(), '');
      await page.waitForFunction(() => !document.querySelector('#mainnetSignupModal button[type=submit]').disabled);
      await page.screenshot({ path: path.join(fixtures, `form-${viewport.width}-${colorScheme}.png`) });
      assert.deepEqual(errors, []);
      await page.close();
    }
    assert.ok(challengeCount >= 4);
    for (const { url, form } of submitted) {
      assert.equal(url, '/api/v1/mainnet-signup');
      assert.equal(form.get('email'), 'player@example.com');
      const nonce = Buffer.alloc(8);
      nonce.writeBigUInt64BE(BigInt(form.get('pow_nonce')));
      const hash = createHash('sha256').update(Buffer.from(form.get('pow_challenge'), 'base64url')).update(nonce).digest();
      assert.equal(hash[0], 0, 'the browser solved a real 8-bit proof');
    }
    const plain = await browser.newPage({ javaScriptEnabled: false });
    await plain.goto(origin + '/mainnet-signup');
    await plain.locator('main [name=email]').fill('nojs@example.com');
    await plain.locator('main button[type=submit]').click();
    await plain.getByRole('status').waitFor();
    assert.equal(submitted.at(-1).url, '/mainnet-signup');
    assert.equal(submitted.at(-1).form.get('email'), 'nojs@example.com');
    assert.equal(submitted.at(-1).form.get('pow_challenge'), '');
    await plain.close();
  } finally {
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
