import { test, expect } from "@playwright/test";

/**
 * E2E tests for the coordinator frontend UI.
 *
 * These tests cover:
 * 1. Basic UI navigation and page load
 * 2. Authentication flow (registration and login)
 * 3. Competition viewing
 * 4. Entry form display (up to submission, before payment)
 *
 * Prerequisites:
 * - Coordinator server running (just run)
 *
 * Payment and keymeld integration tests should be done in a
 * larger harness that includes LND, Bitcoin, and Keymeld services.
 */

test.describe("Basic UI", () => {
  test("homepage loads and shows competitions table", async ({ page }) => {
    await page.goto("/");

    // Wait for the main content to load
    await expect(page.locator("h1")).toContainText("Fantasy Weather");

    // The competitions table should be present
    await expect(page.locator("#competitionsDataTable")).toBeVisible();
  });

  test("navigation links are present", async ({ page }) => {
    await page.goto("/");

    // Check navbar items
    await expect(page.locator("#allCompetitionsNavClick")).toBeVisible();
    await expect(page.locator("#allEntriesNavClick")).toBeVisible();
    await expect(page.locator("#payoutsNavClick")).toBeVisible();

    // Auth buttons should be visible when not logged in
    await expect(page.locator("#loginNavClick")).toBeVisible();
    await expect(page.locator("#registerNavClick")).toBeVisible();
  });

  test("login modal opens and has tabs", async ({ page }) => {
    await page.goto("/");

    // Click login button
    await page.locator("#loginNavClick").click();

    // Modal should be visible
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    // Should have private key and extension tabs
    await expect(
      page.locator("#loginModal .tabs li[data-target='privateKeyLogin']"),
    ).toBeVisible();
    await expect(
      page.locator("#loginModal .tabs li[data-target='extensionLogin']"),
    ).toBeVisible();
  });

  test("register modal opens and shows private key", async ({ page }) => {
    await page.goto("/");

    // Click register button
    await page.locator("#registerNavClick").click();

    // Modal should be visible
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    // Private key display should eventually have a value (WASM generates it)
    await expect(page.locator("#privateKeyDisplay")).toBeVisible();

    // Wait for WASM to initialize and generate key
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });
  });
});

test.describe("Authentication", () => {
  test("can register a new account", async ({ page }) => {
    await page.goto("/");

    // Click register
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    // Wait for private key to be generated
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });

    // Copy the private key for later
    const privateKey = await page.locator("#privateKeyDisplay").inputValue();
    expect(privateKey).toMatch(/^nsec1/);

    // Check the "I saved my key" checkbox
    await page.locator("#privateKeySavedCheckbox").check();

    // Click next button
    await page.locator("#registerStep1Button").click();

    // Should show success message or close modal
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("can login with private key", async ({ page }) => {
    // First register to create an account
    await page.goto("/");
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });
    const privateKey = await page.locator("#privateKeyDisplay").inputValue();
    await page.locator("#privateKeySavedCheckbox").check();
    await page.locator("#registerStep1Button").click();
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });

    // Logout
    await page.locator("#logoutNavClick").click();
    await expect(page.locator("#authButtons")).toBeVisible();

    // Now login with the private key
    await page.locator("#loginNavClick").click();
    await page.locator("#loginPrivateKey").fill(privateKey);
    await page.locator("#loginButton").click();

    // Should be logged in
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
  });
});

test.describe("Competitions", () => {
  test.beforeEach(async ({ page }) => {
    // Register and login before each test
    await page.goto("/");
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });
    await page.locator("#privateKeySavedCheckbox").check();
    await page.locator("#registerStep1Button").click();
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
  });

  test("competitions table shows headers", async ({ page }) => {
    // Check table headers
    const headers = page.locator("#competitionsDataTable thead th");
    await expect(headers.nth(0)).toContainText("ID");
    await expect(headers.nth(1)).toContainText("Start Time");
    await expect(headers.nth(4)).toContainText("Status");
    await expect(headers.nth(5)).toContainText("Entry fee");
  });

  test("can navigate to entries page", async ({ page }) => {
    await page.locator("#allEntriesNavClick").click();
    await expect(page.locator("#allEntries")).toBeVisible();
    await expect(page.locator("#entriesDataTable")).toBeVisible();
  });

  test("can navigate to payouts page", async ({ page }) => {
    await page.locator("#payoutsNavClick").click();
    await expect(page.locator("#payouts")).toBeVisible();
  });
});

test.describe("Entry Form", () => {
  test.beforeEach(async ({ page }) => {
    // Register and login
    await page.goto("/");
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });
    await page.locator("#privateKeySavedCheckbox").check();
    await page.locator("#registerStep1Button").click();
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
  });

  test("entry container shows when clicking enter on a competition", async ({
    page,
  }) => {
    // Wait for competitions to load
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    // Check if there are any competitions
    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      // Click the first "Enter" button (last column has Enter/View buttons)
      const enterButton = page.locator(
        "#competitionsDataTable tbody tr:first-child button",
      );

      if ((await enterButton.count()) > 0) {
        await enterButton.first().click();

        // Entry container should become visible
        await expect(page.locator("#entryContainer")).toBeVisible({
          timeout: 5000,
        });

        // Should show the submit button
        await expect(page.locator("#submitEntry")).toBeVisible();

        // Should have a back button
        await expect(page.locator("#backToCompetitions")).toBeVisible();
      }
    } else {
      // No competitions available - this is expected in some test environments
      console.log(
        "No competitions available for entry test - create a competition first",
      );
    }
  });

  test("can go back from entry form", async ({ page }) => {
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      const enterButton = page.locator(
        "#competitionsDataTable tbody tr:first-child button",
      );

      if ((await enterButton.count()) > 0) {
        await enterButton.first().click();
        await expect(page.locator("#entryContainer")).toBeVisible({
          timeout: 5000,
        });

        // Click back button
        await page.locator("#backToCompetitions").click();

        // Should be back to competitions
        await expect(page.locator("#allCompetitions")).toBeVisible();
        await expect(page.locator("#entryContainer")).toHaveClass(/hidden/);
      }
    }
  });

  test("entry form shows weather prediction options", async ({ page }) => {
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      const enterButton = page.locator(
        "#competitionsDataTable tbody tr:first-child button",
      );

      if ((await enterButton.count()) > 0) {
        await enterButton.first().click();
        await expect(page.locator("#entryContainer")).toBeVisible({
          timeout: 5000,
        });

        // Wait for weather options to load
        await page.waitForSelector("#entryContent button", { timeout: 10000 });

        // Should have Over/Par/Under buttons
        const buttons = page.locator("#entryContent button");
        const buttonCount = await buttons.count();

        if (buttonCount > 0) {
          // Check that we have prediction buttons
          const buttonTexts = await buttons.allTextContents();
          const hasOver = buttonTexts.some((t) => t.includes("Over"));
          const hasPar = buttonTexts.some((t) => t.includes("Par"));
          const hasUnder = buttonTexts.some((t) => t.includes("Under"));

          expect(hasOver || hasPar || hasUnder).toBe(true);
        }
      }
    }
  });

  test("can select predictions before submitting", async ({ page }) => {
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      const enterButton = page.locator(
        "#competitionsDataTable tbody tr:first-child button",
      );

      if ((await enterButton.count()) > 0) {
        await enterButton.first().click();
        await expect(page.locator("#entryContainer")).toBeVisible({
          timeout: 5000,
        });

        // Wait for weather options to load
        await page.waitForSelector("#entryContent button", { timeout: 10000 });

        // Click a prediction button
        const predictionButtons = page.locator(
          "#entryContent button.is-outlined",
        );
        const buttonCount = await predictionButtons.count();

        if (buttonCount > 0) {
          await predictionButtons.first().click();

          // Button should become active (lose is-outlined class)
          await expect(predictionButtons.first()).not.toHaveClass(
            /is-outlined/,
          );
          await expect(predictionButtons.first()).toHaveClass(/is-active/);
        }
      }
    }
  });
});
