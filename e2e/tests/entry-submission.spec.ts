import { test, expect, Page } from "@playwright/test";

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

// Helper to generate unique email for each test
function uniqueEmail(): string {
  return `test-${Date.now()}-${Math.random().toString(36).slice(2)}@example.com`;
}

// Helper to register with email
async function registerWithEmail(
  page: Page,
  email: string,
  password: string,
): Promise<void> {
  // Wait for WASM to initialize
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  // Open register modal
  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  // Click email tab (should be default)
  await page.locator(".tabs li[data-target='registerEmail']").click();

  // Fill email registration form
  await page.locator("#registerEmailInput").fill(email);
  await page.locator("#registerPassword").fill(password);
  await page.locator("#registerPasswordConfirm").fill(password);

  // Click step 1 button to generate keys
  await page.locator("#emailRegisterStep1Button").click();

  // Wait for nsec to be displayed (WASM generates it)
  await expect(page.locator("#emailNsecDisplay")).toHaveValue(/^nsec1/, {
    timeout: 15000,
  });

  // Check the "I saved my key" checkbox
  await page.locator("#emailNsecSavedCheckbox").check();

  // Complete registration
  await page.locator("#emailRegisterStep2Button").click();

  // Should be logged in - logout container visible
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

test.describe("Basic UI", () => {
  test("homepage loads and shows competitions table", async ({ page }) => {
    await page.goto("/");

    // Wait for the main content to load
    await expect(page.locator(".navbar-brand strong")).toContainText(
      "Fantasy Weather",
    );

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

    // Should have email and extension tabs
    await expect(
      page.locator("#loginModal .tabs li[data-target='emailLogin']"),
    ).toBeVisible();
    await expect(
      page.locator("#loginModal .tabs li[data-target='extensionLogin']"),
    ).toBeVisible();
  });

  test("register modal opens and has email form", async ({ page }) => {
    await page.goto("/");

    // Click register button
    await page.locator("#registerNavClick").click();

    // Modal should be visible
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    // Should have email and extension tabs
    await expect(
      page.locator("#registerModal .tabs li[data-target='registerEmail']"),
    ).toBeVisible();
    await expect(
      page.locator("#registerModal .tabs li[data-target='registerExtension']"),
    ).toBeVisible();

    // Email form should be visible by default
    await expect(page.locator("#registerEmail")).toBeVisible();
    await expect(page.locator("#registerEmailInput")).toBeVisible();
    await expect(page.locator("#registerPassword")).toBeVisible();
    await expect(page.locator("#registerPasswordConfirm")).toBeVisible();
  });
});

test.describe("Authentication", () => {
  test("can register a new account with email", async ({ page }) => {
    await page.goto("/");

    const email = uniqueEmail();
    const password = "testPassword123!";

    await registerWithEmail(page, email, password);

    // Should show success - logout container visible
    await expect(page.locator("#logoutContainer")).toBeVisible();
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("can login with email after registration", async ({ page }) => {
    // First register to create an account
    await page.goto("/");
    const email = uniqueEmail();
    const password = "testPassword123!";

    await registerWithEmail(page, email, password);

    // Logout
    await page.locator("#logoutNavClick").click();
    await expect(page.locator("#authButtons")).toBeVisible();

    // Now login with email
    await page.locator("#loginNavClick").click();
    await page.locator(".tabs li[data-target='emailLogin']").click();
    await page.locator("#loginEmail").fill(email);
    await page.locator("#loginPassword").fill(password);
    await page.locator("#emailLoginButton").click();

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
    const email = uniqueEmail();
    const password = "testPassword123!";
    await registerWithEmail(page, email, password);
  });

  test("competitions table shows headers", async ({ page }) => {
    // Check table headers
    const headers = page.locator("#competitionsDataTable thead th");
    await expect(headers.nth(0)).toContainText("Status");
    await expect(headers.nth(1)).toContainText("Start");
    await expect(headers.nth(4)).toContainText("Fee");
    await expect(headers.nth(5)).toContainText("Pool");
  });

  test("can navigate to entries page", async ({ page }) => {
    // Already registered and logged in via beforeEach
    await page.locator("#allEntriesNavClick").click();

    // Should show entries page content (allEntries container)
    await expect(page.locator("#allEntries")).toBeVisible({
      timeout: 10000,
    });
  });

  test("can navigate to payouts page", async ({ page }) => {
    // Already registered and logged in via beforeEach
    await page.locator("#payoutsNavClick").click();

    // Should show payouts page content
    await expect(page.locator("#payouts")).toBeVisible({
      timeout: 10000,
    });
  });
});

test.describe("Entry Form", () => {
  test.beforeEach(async ({ page }) => {
    // Register and login
    await page.goto("/");
    const email = uniqueEmail();
    const password = "testPassword123!";
    await registerWithEmail(page, email, password);
  });

  test("entry container shows when clicking enter on a competition", async ({
    page,
  }) => {
    // Wait for competitions to load
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    // Find a competition in Registration status that we can enter
    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();

      // Entry container should become visible
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      // Should show the submit button
      await expect(page.locator("#submitEntry")).toBeVisible();

      // Should have a back button
      await expect(page.locator("#backToCompetitions")).toBeVisible();
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });

  test("can go back from entry form", async ({ page }) => {
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    // Find a competition in Registration status that we can enter
    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      // Click back button - uses HTMX to replace main-content
      await Promise.all([
        page.waitForResponse((resp) => resp.url().includes("/competitions")),
        page.locator("#backToCompetitions").click(),
      ]);

      // Should be back to competitions (HTMX replaces entire main-content)
      await expect(page.locator("#allCompetitions")).toBeVisible();
      // Entry container should no longer exist (replaced by HTMX)
      await expect(page.locator("#entryContainer")).not.toBeVisible();
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });

  test("entry form shows weather prediction options", async ({ page }) => {
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    // Find a competition in Registration status that we can enter
    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      // Wait for weather options to load
      await page.waitForSelector("#entryContent button.pick-button", {
        timeout: 10000,
      });

      // Should have Over/Par/Under buttons
      const buttons = page.locator("#entryContent button.pick-button");
      const buttonCount = await buttons.count();

      if (buttonCount > 0) {
        // Check that we have prediction buttons
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
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 10000,
    });

    // Find a competition in Registration status that we can enter
    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (canEnter) {
      await enterButton.click();
      await expect(page.locator("#entryContainer")).toBeVisible({
        timeout: 5000,
      });

      // Wait for weather options to load - use .pick-button class
      await page.waitForSelector("#entryContent button.pick-button", {
        timeout: 10000,
      });

      // Click a prediction button (pick buttons have is-outlined initially)
      const firstPickButton = page
        .locator("#entryContent button.pick-button")
        .first();

      // Should be outlined initially
      await expect(firstPickButton).toHaveClass(/is-outlined/);

      await firstPickButton.click();

      // Button should become active (lose is-outlined class, gain is-active)
      await expect(firstPickButton).not.toHaveClass(/is-outlined/);
      await expect(firstPickButton).toHaveClass(/is-active/);
    } else {
      console.log("No competitions in Registration status available for entry");
      test.skip();
    }
  });
});
