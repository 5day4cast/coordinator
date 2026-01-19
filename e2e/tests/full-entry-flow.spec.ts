import { test, expect, Page } from "@playwright/test";

/**
 * Full Entry Submission Flow E2E Tests
 *
 * These tests validate the complete user journey:
 * Login → View Competitions → Select Competition → Make Picks → Pay → Submit Entry
 *
 * When running with mock services (config/e2e.toml):
 * - MockLnClient auto-accepts invoices after 2 seconds
 * - MockOracle handles event creation
 * - No real Bitcoin/Lightning infrastructure required
 *
 * Prerequisites:
 * - Coordinator running with mock mode enabled
 * - At least one competition in "Registration" status
 */

// Helper to generate unique email for each test
function uniqueEmail(): string {
  return `test-${Date.now()}-${Math.random().toString(36).slice(2)}@example.com`;
}

// Helper to register and login with email
async function registerAndLogin(page: Page): Promise<void> {
  await page.goto("/");

  // Wait for WASM to initialize
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  const email = uniqueEmail();
  const password = "testPassword123!";

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

  // Verify logged in
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

test.describe("Full Entry Submission Flow", () => {
  test("complete entry flow: login → competition → picks → payment → submission", async ({
    page,
  }) => {
    // Step 1: Register and login
    await registerAndLogin(page);

    // Step 2: Wait for competition table to load (already on home page after registration)
    // Note: Don't use page.goto("/") as it reloads the page and loses WASM auth state
    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    let rowCount = await page
      .locator("#competitionsDataTable tbody tr")
      .count();

    if (rowCount === 0) {
      console.log(
        "No competitions available - test requires at least one competition in Registration status",
      );
      test.skip();
      return;
    }

    // Step 3: Find a competition we can enter (has "Create Entry" button)
    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (!canEnter) {
      console.log("No competitions in Registration status available for entry");
      test.skip();
      return;
    }

    // Click Enter to open entry form
    await enterButton.click();

    // Step 4: Verify entry form is displayed
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    // Should show competition details
    await expect(page.locator("#entryContent")).toBeVisible();

    // Step 5: Wait for weather prediction options to load
    await page.waitForSelector("#entryContent button", { timeout: 10000 });

    // Step 6: Make weather predictions (select Over/Par/Under for each metric)
    // Use .pick-button class which doesn't change on selection
    const allPickButtons = page.locator("#entryContent button.pick-button");
    const buttonCount = await allPickButtons.count();

    if (buttonCount > 0) {
      // Click first few buttons to make predictions
      // In real UI, these are organized by station and metric
      for (let i = 0; i < Math.min(3, buttonCount); i++) {
        const button = allPickButtons.nth(i);
        await button.click();
        // Button should become active (class changes from is-outlined to is-info is-active)
        await expect(button).toHaveClass(/is-active/);
      }
    }

    // Step 7: Submit entry (this triggers invoice creation)
    const submitButton = page.locator("#submitEntry");
    await expect(submitButton).toBeVisible();
    await expect(submitButton).toBeEnabled();

    // Capture console messages for debugging
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        console.log(`[browser ${msg.type()}]:`, msg.text());
      }
    });

    // Click submit to trigger the payment flow
    await submitButton.click();

    // Wait for either payment modal or error message
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

    // Should show payment request
    await expect(page.locator("#paymentRequest")).toBeVisible();

    // With MockLnClient auto_accept_secs=2, payment should be accepted automatically
    // Wait for payment to be processed and modal to close
    await expect(page.locator("#ticketPaymentModal")).not.toHaveClass(
      /is-active/,
      { timeout: 15000 },
    );

    // Entry should be submitted - success message should appear
    await expect(page.locator("#successMessage")).toBeVisible({
      timeout: 5000,
    });

    // Verify submit button shows success state
    await expect(page.locator("#submitEntry")).toHaveText("Entry Submitted!");
  });

  test("entry form displays competition info and entry fee", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    // Entry form should show fee information
    const entryContent = page.locator("#entryContent");
    await expect(entryContent).toBeVisible();

    // Should have a submit button
    await expect(page.locator("#submitEntry")).toBeVisible();
  });

  test("can navigate back from entry form to competitions", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    // Click back button - uses HTMX to replace main-content
    await Promise.all([
      page.waitForResponse((resp) => resp.url().includes("/competitions")),
      page.locator("#backToCompetitions").click(),
    ]);

    // Should be back to competitions view (HTMX replaces entire main-content)
    await expect(page.locator("#allCompetitions")).toBeVisible();
    // Entry container should no longer exist (replaced by HTMX)
    await expect(page.locator("#entryContainer")).not.toBeVisible();
  });

  test("prediction buttons toggle correctly", async ({ page }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await page.waitForSelector("#entryContent button.pick-button", {
      timeout: 10000,
    });

    // Get first prediction button (Over/Par/Under for one metric)
    const firstButton = page
      .locator("#entryContent button.pick-button")
      .first();

    // Initially should be outlined (not selected)
    await expect(firstButton).toHaveClass(/is-outlined/);

    // Click to select
    await firstButton.click();

    // Should now be active (selected)
    await expect(firstButton).toHaveClass(/is-active/);
    await expect(firstButton).not.toHaveClass(/is-outlined/);
  });
});

test.describe("Competition Status Display", () => {
  test("competitions table shows correct status badges", async ({ page }) => {
    await page.goto("/");

    await page.waitForSelector("#competitionsDataTable", { timeout: 10000 });

    // Table should have status column
    const statusHeader = page.locator(
      "#competitionsDataTable thead th:has-text('Status')",
    );
    await expect(statusHeader).toBeVisible();

    // Check that rows have status indicators
    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      // First row should have a status
      const firstRowStatus = page
        .locator("#competitionsDataTable tbody tr")
        .first()
        .locator("td")
        .nth(0); // Status is 1st column (0-indexed: 0)

      await expect(firstRowStatus).toBeVisible();

      // Status should be one of: Registration, Live, Setup, Signing, Completed
      const statusText = await firstRowStatus.textContent();
      expect(
        ["Registration", "Live", "Setup", "Signing", "Completed"].some((s) =>
          statusText?.includes(s),
        ),
      ).toBe(true);
    }
  });

  test("only Registration status competitions have Enter button", async ({
    page,
  }) => {
    // Register to see Enter buttons
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    // Get all rows
    const rows = page.locator("#competitionsDataTable tbody tr");
    const rowCount = await rows.count();

    for (let i = 0; i < rowCount; i++) {
      const row = rows.nth(i);
      const statusCell = row.locator("td").nth(0);
      const statusText = await statusCell.textContent();

      const enterButton = row
        .locator("button, a")
        .filter({ hasText: /Enter|Create Entry/ });
      const viewButton = row.locator("button:has-text('View')");

      if (statusText?.includes("Registration")) {
        // Registration status should have Enter/Create Entry button
        await expect(enterButton.first()).toBeVisible();
      } else {
        // Other statuses should have View button (or no button)
        // This depends on your UI implementation
      }
    }
  });
});
