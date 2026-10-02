// The discovery page's initial-result polling, using the vendored HTMX and real map script.
// Rust template tests cover the loading/success/failure attributes and canonical filter URL.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const path = require('node:path');
const test = require('node:test');
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright');
const root = path.join(__dirname, '../..');
const htmx = readFileSync(path.join(root, 'vendor/htmx/4.0.0/htmx.min.js'));
const mapScript = readFileSync(path.join(root, 'crates/coordinator/src/templates/admin/weather_map.js'));
const filters = '/admin/competition?day=2026-10-03&weather=wind&location=Portland';

const map = `<form id="game-creation"><input name="entry_fee" value="5000">
<section class="weather-map" data-usable="true"><div class="map-controls">
<select data-map-layer disabled><option value="high">High</option><option value="wind">Wind</option></select>
<select data-map-time disabled><option value="">Whole window</option></select>
<input data-map-wind type="checkbox" checked disabled>
<button type="button" data-map-zoom="0.6">Zoom</button><button type="button" data-map-reset>Reset</button></div>
<div data-map-legend></div><svg viewBox="0 0 10 10"><a data-station="KPDX" data-name="Portland"
 data-high="68" data-low="50" data-wind="8" data-rain="10" data-forecasts="[]"><title>Portland</title><circle r="1"/><path class="map-wind"/></a></svg>
<p data-map-inspector></p><p data-map-count></p></section><div id="map-selections"></div>
<label><input name="locations" type="checkbox" value="KPDX">Portland</label></form>
<script src="/weather-map.js" defer></script>`;

function region(state) {
  const loading = state === 'loading';
  return `<section id="weather-discovery-results" ${loading ? `hx-get="${filters}" hx-trigger="every 2s"` : ''}
 hx-select="#weather-discovery-results" hx-target="this" hx-swap="outerHTML" hx-sync="this:drop" hx-push-url="false" aria-busy="${loading}">
 ${loading ? 'Loading eligible stations and forecasts.' : state === 'failed' ? 'Weather discovery is unavailable.' : map}
 <a href="${filters}">Refresh results</a></section>`;
}
function content(state) {
  return `<main><form action="/admin/competition" method="get"><input name="location" value="Portland"></form>${region(state)}</main>`;
}
function page(state) {
  return `<!doctype html><html><head><script src="/htmx.js" defer></script></head><body>${content(state)}</body></html>`;
}

test('cold results appear, map initializes, polling stops and native fallback works', async () => {
  let state = 'loading', polls = 0, active = 0, maxActive = 0, fail = false;
  const methods = [];
  const server = createServer(async (req, res) => {
    methods.push(req.method);
    if (req.url === '/htmx.js' || req.url === '/weather-map.js') {
      res.setHeader('Content-Type', 'text/javascript');
      return res.end(req.url === '/htmx.js' ? htmx : mapScript);
    }
    res.setHeader('Content-Type', 'text/html');
    if (req.headers['hx-request']) {
      polls++; active++; maxActive = Math.max(maxActive, active);
      // A slow response overlaps a polling tick; the next tick must be dropped.
      await new Promise(resolve => setTimeout(resolve, 2300));
      state = fail ? 'failed' : polls >= 2 ? 'ready' : 'loading';
      active--;
      return res.end(content(state));
    }
    res.end(page(state));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    const p = await browser.newPage(), errors = [];
    p.on('pageerror', e => errors.push(e.message));
    await p.goto(origin + filters);
    await p.locator('input[name=location]').fill('Unsaved filter edit');
    await p.waitForFunction(() => document.querySelector('.weather-map')?.dataset.ready === 'true', null, { timeout: 30000 });
    assert.equal(await p.locator('input[name=location]').inputValue(), 'Unsaved filter edit');
    assert.equal(await p.locator('[data-map-layer]').isEnabled(), true);
    await p.locator('[data-map-layer]').selectOption('wind');
    assert.match(await p.locator('[data-map-legend]').innerText(), /mph/);
    await p.locator('input[name=locations]').check();
    await p.locator('input[name=entry_fee]').fill('7000');
    const completedPolls = polls;
    await p.waitForTimeout(2500);
    assert.equal(polls, completedPolls, 'Ready results must stop polling');
    assert.equal(maxActive, 1, 'Slow result reads must not queue concurrent polls');
    assert.equal(await p.locator('input[name=entry_fee]').inputValue(), '7000');
    assert.equal(await p.locator('input[name=locations]').isChecked(), true);
    assert.deepEqual(errors, []);
    await p.close();

    state = 'loading'; fail = true; polls = 0;
    const failure = await browser.newPage();
    await failure.goto(origin + filters);
    await failure.getByText('Weather discovery is unavailable.', { exact: false }).waitFor();
    const failedPolls = polls;
    await failure.waitForTimeout(2500);
    assert.equal(polls, failedPolls, 'A failed refresh must not poll forever');
    assert.equal(await failure.getByRole('link', { name: 'Refresh results' }).getAttribute('href'), filters);
    await failure.close();

    state = 'loading'; fail = false;
    const native = await browser.newContext({ javaScriptEnabled: false });
    const n = await native.newPage();
    await n.goto(origin + filters);
    const beforeNative = polls;
    state = 'ready';
    await n.getByRole('link', { name: 'Refresh results' }).click();
    assert.equal(await n.locator('input[name=locations]').count(), 1);
    assert.equal(await n.locator('[data-map-layer]').isDisabled(), true);
    assert.equal(polls, beforeNative, 'No-script refresh is an ordinary navigation');
    assert(methods.every(method => method === 'GET'));
    await native.close();
  } finally {
    await browser.close();
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
});
