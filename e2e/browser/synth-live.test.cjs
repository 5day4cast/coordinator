// Real-browser coverage for synth's pages on htmx 4: the SSE push and its catch-up after a
// reconnect, the action buttons, the copy buttons, and the content security policy that stops
// markup from running.
//
// htmx 4 has no switch to stop it running `hx-on` handlers or `<script>` tags in content it swaps
// in, so synth's policy (script-src 'self', nothing inline, no eval, Trusted Types) is what does.
// This serves the vendored htmx and its SSE extension, synth's own scripts, and synth's policy,
// read from the source, and pushes a fragment carrying a script, an inline handler and an `hx-on`
// attribute.
//
// Run from e2e/: node --test browser/synth-live.test.cjs
// With a Chromium other than Playwright's own, set CHROMIUM_EXECUTABLE.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');

const root = path.join(__dirname, '../..');
const read = (file) => readFileSync(path.join(root, file), 'utf8');
const script = [
  read('vendor/htmx/4.0.0/htmx.min.js'),
  read('vendor/htmx/4.0.0/ext/hx-sse.min.js'),
  read('crates/synth/src/server/assets/copy.js'),
  read('crates/synth/src/server/assets/security.js'),
].join('\n;\n');
// The policy as synth's server sends it, joined from its Rust string literal.
const policy = read('crates/synth/src/server/mod.rs')
  .match(/CONTENT_SECURITY_POLICY: &str = "([^"]*)";/)[1]
  .replace(/\\\n\s*/g, '');

// An SSE message carrying `html`, as synth's server sends one.
const message = (html) => `data: ${html.split('\n').join('\ndata: ')}\n\n`;

const fragment = (n) => `
  <p id="pushed">push ${n}</p>
  <script>document.getElementById('pushed').textContent = 'script ran'</script>
  <script>window.injected = 'script'</script>
  <img src="/missing.png" onerror="window.injected = 'onerror'">
  <button id="evil" hx-on:click="window.injected = 'hx-on'">evil</button>
  <details id="step-1" hx-preserve><summary>details</summary><pre>push ${n}</pre></details>
  <button id="copy" class="copy" type="button" data-copy-url="/runs/r1/trail.tsv">Copy as TSV</button>`;

test('pushed fragments swap in, and injected script does not run', async () => {
  const streams = [];
  const reconnects = [];
  const posts = [];
  const server = createServer((req, res) => {
    res.setHeader('Content-Security-Policy', policy);
    if (req.url === '/synth.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(script);
    }
    if (req.url.startsWith('/api/live')) {
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
      streams.push(res);
      // Like synth: every stream opens with its id, and a page that reconnects gets its topic
      // as it is now.
      res.write('id: live\n\n');
      if (req.headers['last-event-id']) {
        reconnects.push(req.headers['last-event-id']);
        return res.write(message('<p id="pushed">caught up</p>'));
      }
      return res.write(message(fragment(1)));
    }
    if (req.url === '/api/run' && req.method === 'POST') {
      posts.push(req.headers['hx-request']);
      res.setHeader('Content-Type', 'text/html');
      return res.end('Started <strong>full_lifecycle</strong>');
    }
    if (req.url === '/runs/r1/trail.tsv') {
      res.setHeader('Content-Type', 'text/tab-separated-values');
      return res.end('step\tstatus\nentry payment · alice\tdone\n');
    }
    if (req.url === '/missing.png') {
      res.statusCode = 404;
      return res.end();
    }
    res.setHeader('Content-Type', 'text/html');
    res.end(`<!doctype html><html><head><script src="/synth.js" defer></script></head><body>
      <button id="run" hx-post="/api/run" hx-target="#action-result" hx-confirm="Start a run?">Run</button>
      <p id="action-result"></p>
      <main id="live" hx-sse:connect="/api/live?topic=run:r1"><p>first render</p></main>
    </body></html>`);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({
    headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}),
  });
  try {
    const context = await browser.newContext({ permissions: ['clipboard-read', 'clipboard-write'] });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.addInitScript(() => {
      window.violations = [];
      document.addEventListener('securitypolicyviolation', (event) => {
        window.violations.push(event.violatedDirective);
      });
    });
    await page.goto(origin);

    // The SSE extension connects and swaps the unnamed message into the page.
    await page.waitForSelector('#pushed');
    assert.equal(await page.textContent('#pushed'), 'push 1');
    await page.click('#evil');
    await page.waitForTimeout(200);
    assert.equal(await page.evaluate(() => window.injected), undefined);
    const violations = await page.evaluate(() => window.violations);
    assert.ok(violations.some((directive) => directive.startsWith('script-src')), violations.join());

    // An open section stays open across pushes.
    await page.click('#step-1 summary');
    streams.forEach((stream) => stream.write(message(fragment(2))));
    await page.waitForFunction(() => document.querySelector('#pushed').textContent === 'push 2');
    assert.equal(await page.evaluate(() => document.querySelector('#step-1').open), true);
    assert.equal(await page.evaluate(() => window.injected), undefined);

    // An action asks first, posts as htmx, and reports back beside the header.
    page.once('dialog', (dialog) => dialog.accept());
    await page.click('#run');
    await page.waitForFunction(() => document.querySelector('#action-result').textContent.includes('Started'));
    assert.deepEqual(posts, ['true']);

    // A copy button fetches the export it names.
    await page.click('#copy');
    await page.waitForFunction(() => document.querySelector('#copy').textContent === 'copied');
    assert.match(await page.evaluate(() => navigator.clipboard.readText()), /entry payment · alice/);

    // Trusted Types: page script cannot turn a string into HTML; only htmx's policy can.
    const sink = await page.evaluate(() => {
      try {
        document.getElementById('action-result').innerHTML = '<b>injected</b>';
        return 'assigned';
      } catch (error) {
        return error.name;
      }
    });
    assert.equal(sink, 'TypeError');

    // A page whose stream drops reconnects, says it was away, and catches up.
    streams.forEach((stream) => stream.end());
    await page.waitForFunction(() => document.querySelector('#pushed').textContent === 'caught up');
    assert.deepEqual(reconnects, ['live']);

    // htmx 4 keeps no page history in storage.
    assert.deepEqual(await page.evaluate(() => ({ ...localStorage })), {});
    assert.deepEqual(await page.evaluate(() => ({ ...sessionStorage })), {});
    assert.deepEqual(errors, []);
  } finally {
    streams.forEach((stream) => stream.end());
    await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
