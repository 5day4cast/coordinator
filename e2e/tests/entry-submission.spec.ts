import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerWithUsername(
  page: Page,
  username: string,
  password: string,
): Promise<void> {
  // The wallet loads on demand (normally when a log-in dialog opens).
  await page.evaluate(() => window.initWasm());

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
}

test.describe("Basic UI", () => {
  test("homepage loads and shows competitions table", async ({ page }) => {
    await page.goto("/");

    await expect(page.locator(".navbar-brand strong")).toContainText(
      "Fantasy Weather",
    );

    await expect(page.locator("#competitions-page")).toBeVisible();
  });

  test("navigation links are present", async ({ page }) => {
    await page.goto("/");

    await expect(page.locator("#allCompetitionsNavClick")).toBeVisible();
    await expect(page.locator("#allEntriesNavClick")).toBeVisible();
    await expect(page.locator("#payoutsNavClick")).toBeVisible();

    await expect(page.locator("#loginNavClick")).toBeVisible();
    await expect(page.locator("#registerNavClick")).toBeVisible();
  });

  test("login modal opens and has tabs", async ({ page }) => {
    await page.goto("/");

    await page.locator("#loginNavClick").click();

    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    await expect(
      page.locator("#loginModal .tabs li[data-target='usernameLogin']"),
    ).toBeVisible();
    await expect(
      page.locator("#loginModal .tabs li[data-target='extensionLogin']"),
    ).toBeVisible();
  });

  test("register modal opens and has username form", async ({ page }) => {
    await page.goto("/");

    await page.locator("#registerNavClick").click();

    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    await expect(
      page.locator("#registerModal .tabs li[data-target='registerUsername']"),
    ).toBeVisible();
    await expect(
      page.locator("#registerModal .tabs li[data-target='registerExtension']"),
    ).toBeVisible();

    await expect(page.locator("#registerUsername")).toBeVisible();
    await expect(page.locator("#registerUsernameInput")).toBeVisible();
    await expect(page.locator("#registerPassword")).toBeVisible();
    await expect(page.locator("#registerPasswordConfirm")).toBeVisible();
  });
});

test.describe("Authentication", () => {
  test("can register a new account with username", async ({ page }) => {
    await page.goto("/");

    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await expect(page.locator("#logoutContainer")).toBeVisible();
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("can login with username after registration", async ({ page }) => {
    await page.goto("/");
    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await page.locator("#logoutNavClick").click();
    await expect(page.locator("#authButtons")).toBeVisible();

    await page.locator("#loginNavClick").click();
    await page.locator(".tabs li[data-target='usernameLogin']").click();
    await page.locator("#loginUsername").fill(username);
    await page.locator("#loginPassword").fill(password);
    await page.locator("#usernameLoginButton").click();

    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
  });
});

test.describe("Competitions", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    const username = uniqueUsername();
    const password = "testPassword123!";
    await registerWithUsername(page, username, password);
  });

  test("competitions table shows headers", async ({ page }) => {
    const headers = page.locator("#competitions-page .competition-header").first().locator("span");
    await expect(headers.nth(0)).toContainText("Status");
    await expect(headers.nth(1)).toContainText("Window");
    await expect(headers.nth(2)).toContainText("Entry");
    await expect(headers.nth(3)).toContainText("Pot");
    await expect(headers.nth(5)).toContainText("Paid places");
  });

  test("can navigate to entries page", async ({ page }) => {
    await page.locator("#allEntriesNavClick").click();

    await expect(page.locator("#allEntries")).toBeVisible({
      timeout: 10000,
    });
  });

  test("can navigate to payouts page", async ({ page }) => {
    await page.locator("#payoutsNavClick").click();

    await expect(page.locator("#payouts")).toBeVisible({
      timeout: 10000,
    });
  });
});

test.describe("Entry Form", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    const username = uniqueUsername();
    const password = "testPassword123!";
    await registerWithUsername(page, username, password);
  });

  test("entry container shows when clicking enter on a competition", async ({
    page,
  }) => {
    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 10000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();

      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      await expect(page.locator("#submitEntry")).toBeVisible();

      await expect(page.locator("#entryContainer .back-link")).toBeVisible();
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });

  test("can go back from entry form", async ({ page }) => {
    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 10000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
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
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });

  test("entry form shows weather prediction options", async ({ page }) => {
    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 10000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      await page.waitForSelector("#entryForm .pick-option", {
        timeout: 10000,
      });

      const buttons = page.locator("#entryForm .pick-option");
      const buttonCount = await buttons.count();

      if (buttonCount > 0) {
        const buttonTexts = await buttons.allTextContents();
        const hasOver = buttonTexts.some((t) => t.includes("Over"));
        const hasPar = buttonTexts.some((t) => t.includes("Par"));
        const hasUnder = buttonTexts.some((t) => t.includes("Under"));

        expect(hasOver || hasPar || hasUnder).toBe(true);
      }
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });

  test("can select predictions before submitting", async ({ page }) => {
    await page.waitForSelector("#competitions-page a.competition-row", {
      timeout: 10000,
    });

    const enterButton = page
      .locator("#competitions-page a.competition-row[href$='/entry-form']")
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      await page.waitForSelector("#entryForm .pick-option", {
        timeout: 10000,
      });

      const row = page.locator("#entryForm .pick-options").first();
      const firstPickButton = row.locator(".pick-option").first();
      const input = firstPickButton.locator("input");
      const noPickButton = row.locator(".pick-option.is-none");
      const noPick = noPickButton.locator("input");
      await expect(noPick).toBeChecked();
      await expect(input).not.toBeChecked();

      await firstPickButton.click();
      await expect(input).toBeChecked();
      await expect(noPick).not.toBeChecked();

      // "No pick" takes the pick back.
      await noPickButton.click();
      await expect(input).not.toBeChecked();
      await expect(noPick).toBeChecked();
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });
});
