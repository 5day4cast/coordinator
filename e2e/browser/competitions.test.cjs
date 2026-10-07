const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { createServer } = require("node:http");
const path = require("node:path");
const test = require("node:test");
const { chromium } = require(process.env.PLAYWRIGHT_CORE || "playwright");
const root = path.join(__dirname, "../..");
const templates = path.join(root, "crates/coordinator/src/templates");

const competitions = [
  { id: "soon", start: 10, fee: 1050, prize: 9000 },
  { id: "cheap", start: 30, fee: 525, prize: 15000 },
  { id: "big", start: 20, fee: 2100, prize: 80000 },
];
function list() {
  return `<div id="competitions-page"><div class="competition-list" data-sortable>
    <div class="competition-header is-sortable"><span>Status</span>
    ${["start", "duration", "fee", "prize", "entries", "action"].map(key =>
      ["start", "fee", "prize"].includes(key)
        ? `<button class="competition-sort" data-sort="${key}">${key}<span class="sort-direction"></span></button>`
        : `<span>${key}</span>`).join("")}</div>
    ${competitions.map(c => `<a href="/competitions/${c.id}/entry-form" class="competition-row"
      data-competition-id="${c.id}" data-start="${c.start}" data-fee="${c.fee}" data-prize="${c.prize}"
      data-facts="Entry fee ${c.fee} sats · Prizes ${c.prize} sats · 12 hours · 3 entered">
      <span class="cell-status">Open</span><span class="cell-window">Wed, 8:00 PM EDT
      </span><span class="cell-duration">12 hours</span><span class="cell-fee">${c.fee} sats</span>
      <span class="cell-win">${c.prize} sats</span><span class="cell-entries"><span class="entry-count">3 entered</span>
      <span class="cell-note player-entries" data-player-entries="${c.id}" data-entry-limit="3">3 max per player</span></span>
      <span class="cell-action">Enter →</span></a>`).join("")}</div></div>`;
}

test("upcoming sorting and personal counts survive refreshes and account changes", { timeout: 30000 }, async () => {
  let requests = 0;
  let delayNext = false;
  let release;
  let failNext = false;
  const server = createServer(async (req, res) => {
    if (req.url === "/competitions/entry-counts") {
      requests++;
      const owner = req.headers.authorization;
      if (delayNext) {
        delayNext = false;
        await new Promise(resolve => { release = resolve; });
      }
      if (failNext) {
        failNext = false;
        res.statusCode = 503;
        return res.end("Unavailable");
      }
      res.setHeader("Content-Type", "application/json");
      return res.end(JSON.stringify(owner === "owner" ? { soon: 2 } : { big: 1 }));
    }
    if (req.url === "/list") return res.end(list());
    res.end(`<!doctype html><html><head><style>
      ${readFileSync(path.join(root, "vendor/bulma/1.0.2/bulma.min.css"))}
      ${readFileSync(path.join(templates, "pages/competitions/competitions.css"))}
      </style></head><body><main id="main-content">${list()}</main><script>
      let signedIn = false;
      const session = { nostrClient: null };
      function isLoggedIn() { return signedIn; }
      function login(owner) {
        signedIn = true;
        session.nostrClient = { getAuthHeader: async () => owner };
        document.body.dispatchEvent(new Event("fw:login"));
      }
      function logout() {
        signedIn = false; session.nostrClient = null;
        document.body.dispatchEvent(new Event("fw:logout"));
      }
      async function refreshList() {
        document.querySelector("#main-content").innerHTML = await (await fetch("/list")).text();
        document.dispatchEvent(new CustomEvent("htmx:after:swap"));
      }
      ${readFileSync(path.join(templates, "shared/authorized_client.js"))}
      ${readFileSync(path.join(templates, "pages/competitions/competitions.js"))}
      setupCompetitions();
      </script></body></html>`);
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const browser = await chromium.launch({ headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    const order = () => page.locator(".competition-row").evaluateAll(rows => rows.map(r => r.dataset.competitionId));
    assert.deepEqual(await order(), ["soon", "big", "cheap"]);
    assert.equal(requests, 0, "Signed-out visits do not ask for private data");
    await page.locator('[data-sort="start"]').click();
    assert.deepEqual(await order(), ["cheap", "big", "soon"]);
    await page.locator('[data-sort="fee"]').click();
    assert.deepEqual(await order(), ["cheap", "soon", "big"]);
    await page.locator('[data-sort="fee"]').click();
    assert.deepEqual(await order(), ["big", "soon", "cheap"]);
    await page.locator('[data-sort="prize"]').click();
    assert.deepEqual(await order(), ["big", "cheap", "soon"]);
    await page.locator('[data-sort="prize"]').click();
    assert.deepEqual(await order(), ["soon", "cheap", "big"]);

    await page.evaluate(() => login("owner"));
    await page.getByText("Your entries: 2 / 3").waitFor();
    assert.equal(await page.getByText("Your entries: 0 / 3").count(), 2);
    await page.evaluate(() => refreshList());
    assert.deepEqual(await order(), ["soon", "cheap", "big"]);
    assert.equal(await page.getByText("Your entries: 2 / 3").count(), 1);
    assert.equal(requests, 1, "Public refresh reuses counts without another signature");

    for (const width of [320, 375, 768, 1280]) {
      await page.setViewportSize({ width, height: 900 });
      assert.equal(await page.locator('[data-sort="prize"]').isVisible(), true);
      assert.equal(await page.getByText("Your entries: 2 / 3").isVisible(), true);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }

    await page.evaluate(() => logout());
    assert.equal(await page.getByText("Your entries:", { exact: false }).count(), 0);
    delayNext = true;
    await page.evaluate(() => login("owner"));
    while (!release) await new Promise(resolve => setTimeout(resolve, 10));
    await page.evaluate(() => login("other"));
    await page.getByText("Your entries: 1 / 3").waitFor();
    release();
    await page.waitForTimeout(100);
    assert.equal(await page.getByText("Your entries: 2 / 3").count(), 0, "Stale account response stays hidden");

    await page.evaluate(() => logout());
    failNext = true;
    await page.evaluate(() => login("owner"));
    await page.getByText("Your count unavailable", { exact: false }).first().waitFor();
    assert.equal(await page.getByText("Your entries: 0 / 3").count(), 0, "Errors are not displayed as zero entries");
    assert.deepEqual(errors, []);
  } finally {
    release?.();
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});

test("full weather labels and numeric pick ranges fit narrow dialogs", { timeout: 30000 }, async () => {
  const browser = await chromium.launch({ headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
  try {
    const page = await browser.newPage();
    const styles = [
      path.join(root, "vendor/bulma/1.0.2/bulma.min.css"),
      path.join(templates, "static/styles.css"),
      path.join(templates, "fragments/picks.css"),
    ].map(file => readFileSync(file, "utf8")).join("\n");
    const picks = [
      ["Highest temperature", "67.4–70.2°F", "69°F"],
      ["Lowest temperature", "&lt; 41.2°F", "40°F"],
      ["Highest wind speed", "&gt; 18.0 kt", "20 knots"],
    ];
    await page.setContent(`<style>${styles}</style><div id="entryScore" class="modal is-active">
      <div class="modal-background"></div><div class="modal-content"><div class="box"><div class="picks-detail">
      <h2 class="title is-5">Your picks</h2><div class="scored-pick picks-header">
      <span>Reading</span><span>Pick</span><span>Observed</span><span>Points</span></div>
      <h3 class="picks-station-name" title="Weather station: New York/JFK International, NY (KJFK)">New York, NY</h3>
      ${picks.map(([metric, choice, reading]) => `<div class="scored-pick is-hit">
      <span class="pick-metric">${metric}</span><span class="pick-choice"><span class="pick-target">${choice}</span></span>
      <span class="pick-reading">${reading}</span><span class="pick-result">✓ +10
      <span class="pick-state is-final">Final</span></span></div>`).join("")}</div></div></div></div>`);
    for (const width of [320, 375, 768]) {
      await page.setViewportSize({ width, height: 900 });
      const overflow = await page.locator(".scored-pick:not(.picks-header) > span").evaluateAll(cells =>
        cells.filter(cell => cell.scrollWidth > cell.clientWidth + 1).map(cell => cell.textContent));
      assert.deepEqual(overflow, [], `Pick cells must fit at ${width}px`);
    }
  } finally {
    await browser.close();
  }
});
