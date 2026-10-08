import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerAndLogin(page: Page): Promise<string> {
  await page.goto("/");

  // The wallet loads on demand (normally when a log-in dialog opens).
  await page.evaluate(() => window.initWasm());

  const username = uniqueUsername();
  const password = "testPassword123!";

  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  await page.locator(".tabs li[data-target='registerUsername']").click();

  await page.locator("#registerUsernameInput").fill(username);
  await page.locator("#registerPassword").fill(password);
  await page.locator("#registerPasswordConfirm").fill(password);
  await page.locator("#registerLightningAddress").fill(`${username}@mock-wallet.dev`);

  await page.locator("#usernameRegisterStep1Button").click();

  await expect(page.locator("#usernameNsecDisplay")).toHaveValue(/^nsec1/, {
    timeout: 15000,
  });

  await page.locator("#usernameNsecSavedCheckbox").check();

  await page.locator("#usernameRegisterStep2Button").click();

  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
  return username;
}

async function chooseRequiredPicks(page: Page): Promise<void> {
  const form = page.locator("#entryForm");
  const required = Number(await form.getAttribute("data-max-values"));
  expect(required).toBeGreaterThan(0);
  const categories = form.locator(".pick-options");
  await expect(categories.nth(required - 1)).toBeVisible();
  for (let i = 0; i < required; i++) {
    await categories.nth(i).locator(".pick-option").first().click();
  }
  await expect(form.locator("input[type=radio]:checked")).toHaveCount(required);
}

async function expectPaymentHelp(page: Page, messages: string[]): Promise<void> {
  const link = page.getByRole("link", { name: "How your entry is held and paid", exact: true });
  const href = await link.getAttribute("href");
  expect(href).toMatch(/^\/help\?open=advanced&competition=[^#]+#advanced$/);
  const popup = page.waitForEvent("popup");
  await link.click();
  const help = await popup;
  try {
    await expect(help).toHaveURL(new URL(href!, page.url()).href);
    await expect(help.locator("#advanced > details")).toHaveAttribute("open", "");
    for (const message of messages) {
      await expect(help.locator("#advanced")).toContainText(message);
    }
  } finally {
    await help.close();
  }
}

test.describe("Full Entry Submission Flow", () => {
  test("complete entry flow: login → competition → picks → payment → submission", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 15000,
    });

    let rowCount = await page
      .locator("#competitions-page a.competition-row")
      .count();

    if (rowCount === 0) {
      console.log(
        "No competitions available - test requires at least one competition in Registration status",
      );
      test.skip();
      return;
    }

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (!canEnter) {
      console.log("No competitions in Registration status available for entry");
      test.skip();
      return;
    }

    await enterButton.click();

    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await expect(page.locator("#entryForm")).toBeVisible();

    await page.waitForSelector("#entryForm .pick-option", { timeout: 10000 });

    await chooseRequiredPicks(page);

    const submitButton = page.locator("#submitEntry");
    await expect(submitButton).toBeVisible();
    await expect(submitButton).toBeEnabled();

    // Entering is the consent: no checkbox, one line about where money goes.
    await expect(page.locator("#entryPayoutDestination")).toContainText("submit a Lightning invoice");
    await expect(page.locator("#entryContainer input[type=checkbox]")).toHaveCount(0);
    await expectPaymentHelp(page, ["no payout escrow"]);
    const ticketRequests: string[] = [];
    page.on("request", (request) => {
      if (request.method() === "POST" && /\/competitions\/[^/]+\/ticket$/.test(request.url())) {
        ticketRequests.push(request.url());
      }
    });

    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        console.log(`[browser ${msg.type()}]:`, msg.text());
      }
    });

    await submitButton.click();

    await Promise.race([
      expect(page.locator("#ticketPaymentModal")).toHaveClass(/is-active/, {
        timeout: 15000,
      }),
      page
        .locator("#errorMessage:not(.hidden)")
        .waitFor({ timeout: 15000 })
        .then(async () => {
          const errorText = await page.locator("#errorMessage").textContent();
          throw new Error(`Entry submission failed: ${errorText}`);
        }),
    ]);

    await expect(page.locator("#paymentQR")).toBeVisible();

    await expect(page.locator("#ticketPaymentModal")).not.toHaveClass(
      /is-active/,
      { timeout: 15000 },
    );

    await expect(page.locator("#successMessage")).toBeVisible({
      timeout: 5000,
    });

    await expect(page.locator("#submitEntry")).toHaveText("Entered");
    expect(ticketRequests).toHaveLength(1);
  });

  test("entry form displays competition info and entry fee", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    const entryContent = page.locator("#entryForm");
    await expect(entryContent).toBeVisible();

    await expect(page.locator("#submitEntry")).toBeVisible();
  });

  test("automatic payout consent refuses a ticket without the approved escrow policy", async ({ page }) => {
    const username = await registerAndLogin(page);
    const profileAddress = `${username}@mock-wallet.dev`.toLowerCase();
    await page.evaluate(() => { document.body.dataset.oracleBase = window.location.origin; });
    await page.route("**/api/v1/competitions/*/payout-terms", (route) => route.fulfill({
      json: { enabled: true, relative_locktime_block_delta: 72, max_fee_rate_sat_vb: 5 },
    }));
    await page.route("**/oracle/events/*", (route) => route.fulfill({
      json: { id: new URL(route.request().url()).pathname.split("/").pop(), event_announcement: {} },
    }));
    let ticketRequest: Record<string, any> | undefined;
    await page.route("**/api/v1/competitions/*/ticket", (route) => {
      ticketRequest = route.request().postDataJSON();
      // A substituted legacy ticket must not reach the payment QR or downgrade enrollment.
      return route.fulfill({ json: {
        ticket_id: "00000000-0000-7000-8000-000000000001",
        payment_request: "invoice-that-must-never-be-displayed",
        keymeld_session_id: null,
        keymeld_registration: null,
      } });
    });
    await page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first().click();
    // Winnings go to the profile's address; the entry form asks nothing more.
    await expect(page.locator("#entryContainer input[type=checkbox]")).toHaveCount(0);
    // The linked help page opens this competition's payment terms.
    await expectPaymentHelp(page, [
      "On-chain fees for the contract are capped at 100 sat/vB",
      "Winner shares by rank: 70%, 30%",
    ]);
    // Beside the prize, how the two places share the pot; no fee breakdown.
    await expect(page.locator("#entryContainer .entry-facts")).toContainText("1st 70% · 2nd 30%");
    await chooseRequiredPicks(page);
    await page.locator("#submitEntry").click();
    await expect(page.locator("#errorMessage")).toContainText("The ticket omitted the approved payout escrow policy");
    await expect(page.locator("#ticketPaymentModal")).not.toHaveClass(/is-active/);
    await expect(page.locator("#paymentQR")).toHaveCount(0);
    await expect(page.locator("#walletLinks a[href*='invoice-that-must-never-be-displayed']")).toHaveCount(0);
    expect(ticketRequest?.payout.lightning_address).toBe(profileAddress);
    expect(ticketRequest?.payout.payout_hash).toMatch(/^[0-9a-f]{64}$/);
  });

  test("can navigate back from entry form to competitions", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await Promise.all([
      page.waitForResponse((resp) => resp.url().includes("/competitions")),
      page.locator("#entryContainer .back-link").click(),
    ]);

    await expect(page.locator("#competitions-page")).toBeVisible();
    await expect(page.locator("#entryContainer")).not.toBeVisible();
  });

  test("a pick is taken back by choosing it again", async ({ page }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await page.waitForSelector("#entryForm .pick-option", {
      timeout: 10000,
    });

    const row = page.locator("#entryForm .pick-row").first();
    const [first, second] = [row.locator(".pick-option").nth(0), row.locator(".pick-option").nth(1)];
    const checked = row.locator("input:checked");
    await expect(checked).toHaveCount(0);

    await first.click();
    await expect(first.locator("input")).toBeChecked();

    await second.click();
    await expect(second.locator("input")).toBeChecked();
    await expect(first.locator("input")).not.toBeChecked();

    await second.click();
    await expect(checked).toHaveCount(0);

    // Space on the focused pick takes it back too.
    await first.click();
    await first.locator("input").press("Space");
    await expect(checked).toHaveCount(0);
  });
});

test.describe("Competition Status Display", () => {
  test("competitions are grouped with a status badge on every row", async ({ page }) => {
    await page.goto("/");

    await page.waitForSelector("#competitions-page", { timeout: 10000 });
    await expect(page.locator("#competitions-page .intro")).toContainText("Daily Fantasy Weather");

    const rows = page.locator("#competitions-page a.competition-row");
    const count = await rows.count();
    for (let i = 0; i < count; i++) {
      const status = await rows.nth(i).locator(".cell-status").textContent();
      expect(
        ["Open", "Full", "Entries closed", "Live", "Awaiting results", "Finished", "Didn't run", "Cancelled"].some((s) =>
          status?.includes(s),
        ),
      ).toBe(true);
    }
  });

  test("only open competitions link to the entry form", async ({ page }) => {
    await page.goto("/");
    await page.waitForSelector("#competitions-page", { timeout: 10000 });

    const rows = page.locator("#competitions-page a.competition-row");
    const count = await rows.count();
    for (let i = 0; i < count; i++) {
      const row = rows.nth(i);
      const status = await row.locator(".cell-status").textContent();
      const href = await row.getAttribute("href");
      expect(href?.endsWith("/entry-form")).toBe(status?.trim() === "Open");
    }
  });

  test("the finished tab lists every finished competition with a search", async ({ page }) => {
    await page.goto("/competitions?show=finished");
    await expect(page.locator("#competitions-page .competition-tabs [aria-current='page']")).toContainText("Finished");
    const search = page.locator("#competitions-page form.competition-search input[name='q']");
    await search.fill("no-such-competition");
    await search.press("Enter");
    await expect(page.locator("#competitions-page .empty-state")).toContainText("Nothing matches");
    await expect(page).toHaveURL(/show=finished.*q=no-such-competition/);
    await page.locator("#competitions-page .competition-tabs a", { hasText: "Open & recent" }).click();
    await expect(page.locator("#competitions-page .intro")).toContainText("Daily Fantasy Weather");
  });

  test("an account page opened by its address offers the log-in dialog", async ({ page }) => {
    await page.goto("/entries");
    await expect(page.locator(".sign-in-required")).toContainText("You're signed out");
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);
    await expect(page.locator("nav.navbar")).toBeVisible();
  });
});
