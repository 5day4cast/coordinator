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

    const allPickButtons = page.locator("#entryForm .pick-option");
    const buttonCount = await allPickButtons.count();

    if (buttonCount > 0) {
      for (let i = 0; i < Math.min(3, buttonCount); i++) {
        const button = allPickButtons.nth(i);
        await button.click();
        await expect(button.locator("input")).toBeChecked();
      }
    }

    const submitButton = page.locator("#submitEntry");
    await expect(submitButton).toBeVisible();
    await expect(submitButton).toBeEnabled();

    // Entering is the consent: no checkbox, one line about where money goes.
    await expect(page.locator("#entryPayoutDestination")).toContainText("submit a Lightning invoice");
    await expect(page.locator("#entryContainer input[type=checkbox]")).toHaveCount(0);
    await expect(page.locator("#entryContainer details.entry-advanced")).toContainText(
      "no payout escrow",
    );
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

    await expect(page.locator("#paymentRequest")).toBeVisible();

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
    await page.locator("#entryForm .pick-option").first().click();
    await page.locator("#submitEntry").click();
    await expect(page.locator("#errorMessage")).toContainText("The ticket omitted the approved payout escrow policy");
    await expect(page.locator("#ticketPaymentModal")).not.toHaveClass(/is-active/);
    await expect(page.locator("#paymentRequest")).not.toHaveValue("invoice-that-must-never-be-displayed");
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

  test("prediction buttons toggle correctly", async ({ page }) => {
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

    const firstButton = page
      .locator("#entryForm .pick-option")
      .first();

    const input = firstButton.locator("input");
    await expect(input).not.toBeChecked();

    await firstButton.click();
    await expect(input).toBeChecked();

    await firstButton.click();
    await expect(input).not.toBeChecked();
  });
});

test.describe("Competition Status Display", () => {
  test("competitions are grouped with a status badge on every row", async ({ page }) => {
    await page.goto("/");

    await page.waitForSelector("#competitions-page", { timeout: 10000 });
    await expect(page.locator("#competitions-page .intro")).toContainText("win the pot");

    const rows = page.locator("#competitions-page a.competition-row");
    const count = await rows.count();
    for (let i = 0; i < count; i++) {
      const status = await rows.nth(i).locator(".cell-status").textContent();
      expect(
        ["Open", "Full", "Live", "Awaiting results", "Finished", "Didn't fill", "Cancelled", "Failed"].some((s) =>
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

  test("an account page opened by its address offers the log-in dialog", async ({ page }) => {
    await page.goto("/entries");
    await expect(page.locator(".sign-in-required")).toContainText("signs you out");
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);
    await expect(page.locator("nav.navbar")).toBeVisible();
  });
});
